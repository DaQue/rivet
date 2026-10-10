//! History commands: `log`, `show` and `diff`.
//!
//! These are the three read-only views over history. `log` walks commits, `show` reads one
//! object in full, and `diff` compares any two of the four states a path can be in. None of them
//! writes to the repository, and all three produce output that the differential tests compare
//! against `git`'s.
//!
//! *Phase 4 for `log`, Phases 4 and 7 for `show`, Phase 8 for `diff`.*

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{DiffAlgorithmArg, GlobalArgs};

/// Arguments to `rivet log`.
///
/// # Caveats
///
/// * **`first_parent` changes which commits exist, not just which are shown.** A `--first-parent`
///   walk over a history with merges visits a different set of commits than a full walk with the
///   merge commits' second parents filtered out, because it also skips the *ancestors* reachable
///   only through those parents. The two are not interchangeable and a filter applied after the
///   walk is wrong.
/// * **`reverse` is order-of-output, and it is defined only for `--date-order`-style walks.**
///   Reversing a topological sort produces a topological sort in the other direction, which is
///   fine; reversing a `--date-order` walk is what Git does and is not, strictly, a valid
///   ordering. It is preserved because a user asking for `--reverse` wants the first commit
///   first, not a guarantee.
/// * **`all` is not "every ref"** — it is every ref *and* `HEAD`, including remote-tracking and
///   tag refs, which can pull in commits no branch reaches. A log that used only
///   `refs/heads/*` silently omits a fetched-into-a-tag history.
/// * **`max_count` is applied after `skip`**, so `-n 10 --skip 5` prints commits 6 to 15. The
///   other order is a common and quiet bug: it prints 5 to 10.
/// * **`since` and `until` compare the *committer* date**, not the author date, unless the
///   `--author-date-order` flag is present. A commit with a rewritten author date is filtered
///   differently by Git and by a caller who assumed the author field, and both results look
///   plausible.
/// * **`paths` filtering is a simplification and must be honest about it.** Git's history
///   simplification walks parents until the tree changes; a naive "include a commit whose diff
///   touches the path" walk shows a different set for merge commits. The plan's Phase 4 does not
///   require the simplification, but the doc comment here must say which one was chosen.
#[derive(Debug, clap::Args)]
pub struct LogArgs {
    /// The revision, range or ref to start from. Defaults to `HEAD`.
    #[arg(value_name = "REVISION", default_value = "HEAD")]
    pub revision: String,

    /// Show only the named paths.
    #[arg(value_name = "PATHSPEC", last = true)]
    pub paths: Vec<PathBuf>,

    /// Stop after this many commits, applied after `--skip`.
    #[arg(short = 'n', long = "max-count", value_name = "NUMBER")]
    pub max_count: Option<usize>,

    /// Skip this many commits before printing any.
    #[arg(long = "skip", value_name = "NUMBER")]
    pub skip: Option<usize>,

    /// One line per commit: the abbreviated id and the summary.
    #[arg(long = "oneline")]
    pub oneline: bool,

    /// Follow only the first parent of each merge.
    #[arg(long = "first-parent")]
    pub first_parent: bool,

    /// Print the commits in the reverse of the chosen order.
    #[arg(long = "reverse")]
    pub reverse: bool,

    /// Walk the commits of every ref, not only the `HEAD` history.
    #[arg(long = "all")]
    pub all: bool,

    /// Sort by committer date — the default.
    #[arg(long = "date-order")]
    pub date_order: bool,

    /// Sort topologically, so that no parent precedes its child.
    #[arg(long = "topo-order")]
    pub topo_order: bool,

    /// Sort by author date.
    #[arg(long = "author-date-order")]
    pub author_date_order: bool,

    /// Limit to commits made after this date.
    #[arg(long = "since", value_name = "DATE", alias = "after")]
    pub since: Option<String>,

    /// Limit to commits made before this date.
    #[arg(long = "until", value_name = "DATE", alias = "before")]
    pub until: Option<String>,

    /// Print a diff of each commit against its first parent.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Print a one-line diffstat after each commit.
    #[arg(long = "stat")]
    pub stat: bool,

    /// Print the diff in `--name-status` form after each commit.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Print the object names in full, not abbreviated.
    #[arg(long = "no-abbrev-commit")]
    pub no_abbrev_commit: bool,

    /// Print the decorate ref names beside each commit.
    #[arg(long = "decorate", value_name = "WHEN")]
    pub decorate: Option<Option<String>>,

    /// Print the commit graph's graph, as `git log --graph` does.
    #[arg(long = "graph")]
    pub graph: bool,
}

/// Arguments to `rivet show`.
///
/// # Caveats
///
/// * **The command's output depends on the object's kind, and it is four commands in one
///   costume.** A commit prints its header and diff, a tag prints the tag object and then the
///   object it points at, a tree prints its entries as `ls-tree` does, and a blob prints its
///   bytes — which may be binary. A handler that assumed "commit" produces nonsense for the
///   other three, and one that refuses them is not `show`.
/// * **A blob's bytes go to stdout *unmodified*.** No trailing newline is added and no encoding
///   is applied; piping `rivet show HEAD:file` into `cmp` must produce a byte-identical file,
///   which is the primary use of this form.
/// * **`path` with a revision is the `REV:path` syntax in a friendlier spelling** —
///   `rivet show HEAD:src/main.rs` and `rivet show HEAD src/main.rs` are the same request. Both
///   must resolve to the same object, including for a path with a colon in its name, which is
///   why the split happens at the *first* colon after a valid revision rather than the last.
/// * **A tag is peeled, and the peeling is shown.** An annotated tag prints the tag object
///   (`tag v1.0`, the tagger, the message) and then the commit it points at; a lightweight tag
///   prints only the commit. Printing the tag object alone looks like a missing commit.
#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// The object to show: a revision, a `REV:path`, a tag name or an object id.
    #[arg(value_name = "OBJECT", default_value = "HEAD")]
    pub object: String,

    /// The paths to show, as an alternative to the `REV:path` spelling.
    #[arg(value_name = "PATHSPEC", last = true)]
    pub paths: Vec<PathBuf>,

    /// Print the object's raw bytes rather than a formatted view.
    #[arg(long = "pretty", value_name = "FORMAT")]
    pub pretty: Option<String>,

    /// Print no diff, only the object's own contents.
    #[arg(short = 's', long = "no-patch")]
    pub no_patch: bool,

    /// Print this many lines of context in the diff.
    #[arg(short = 'U', long = "unified", value_name = "LINES")]
    pub context: Option<usize>,

    /// Print the diff instead of the commit header.
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// Abbreviate the object ids to this many hex characters, or to all of them.
    #[arg(long = "abbrev", value_name = "N")]
    pub abbrev: Option<Option<usize>>,
}

/// Arguments to `rivet diff`.
///
/// # Caveats
///
/// * **The number and meaning of the positionals decides the comparison**, and there are four
///   cases: none (work tree against index), one (work tree against that commit), two (those
///   commits against each other), and `--cached` with none or one (index against `HEAD` or
///   against the named commit). A handler must resolve the case explicitly and refuse the
///   combinations that are not one of the four, rather than defaulting.
/// * **`--cached` and `--staged` are the same flag**, and the grammar declares both. A user who
///   sets both must not get the comparison applied twice.
/// * **`--quiet` is the global flag**, because a subcommand cannot redeclare one that the top
///   level already propagates; see [`crate::cli::GlobalArgs`]. For `diff` it carries its Git
///   meaning on top of the global one — no output, and exit `1` when there are differences —
///   which is why the handler reads `global.quiet` and not a field of its own.
/// * **`quiet` must not compute the diff it is suppressing.** The whole point of `--quiet` is to
///   stop at the first difference on a large tree; producing the full diff and then discarding
///   it makes a script that polls it quadratic.
/// * **`exit_code` makes the exit code report differences**, per [`run_diff`]'s exit codes. It is
///   off by default because `rivet diff` on a dirty tree is a normal, successful operation.
/// * **`output` writes the diff to a file instead of stdout, and there is a case for both.**
///   `--output` is what `git diff --output=x` does; the file is created even when the diff is
///   empty, and a failure to create it is fatal *before* any diff is computed.
/// * **`ignore_space_at_eol` and the other whitespace flags change which hunks exist**, not
///   merely which lines are emphasised. Two files that differ only in trailing whitespace
///   produce *no* hunks under it, which is a materially different answer from "hunks with no
///   visible changes" and must not be implemented as the latter.
/// * **`relative` makes paths relative to the current directory rather than the work tree
///   root.** Git's `--relative` is off by default, so the default output of a diff run from a
///   subdirectory names the full path from the work tree, which surprises people; it is
///   nevertheless the correct default to match.
#[derive(Debug, clap::Args)]
pub struct DiffArgs {
    /// The commits to compare: none, one or two.
    #[arg(value_name = "COMMIT")]
    pub revisions: Vec<String>,

    /// The paths to limit the comparison to.
    #[arg(value_name = "PATHSPEC", last = true)]
    pub paths: Vec<PathBuf>,

    /// Compare the index against `HEAD` instead of the work tree against the index.
    #[arg(long = "cached", alias = "staged")]
    pub cached: bool,

    /// Print a one-line diffstat instead of the diff.
    #[arg(long = "stat")]
    pub stat: bool,

    /// Print only the changed paths, with a letter for the change.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Print only the changed paths.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Exit `1` when there are differences, as `--quiet` does, without suppressing output.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Print this many lines of context.
    #[arg(short = 'U', long = "unified", value_name = "LINES")]
    pub context: Option<usize>,

    /// Choose the diff algorithm: `myers` or `histogram`.
    #[arg(long = "diff-algorithm", value_enum, value_name = "ALGORITHM")]
    pub algorithm: Option<DiffAlgorithmArg>,

    /// Detect a binary file by its contents and say so instead of printing its diff.
    #[arg(long = "binary")]
    pub binary: bool,

    /// Treat every file as text, even one containing a NUL byte.
    #[arg(short = 'a', long = "text")]
    pub text: bool,

    /// Ignore changes in the amount of whitespace.
    #[arg(short = 'w', long = "ignore-all-space")]
    pub ignore_all_space: bool,

    /// Ignore changes in the amount of whitespace, but not a change in whether there is any.
    #[arg(short = 'b', long = "ignore-space-change")]
    pub ignore_space_change: bool,

    /// Ignore trailing whitespace when comparing lines.
    #[arg(long = "ignore-space-at-eol")]
    pub ignore_space_at_eol: bool,

    /// Write the diff to a file rather than to standard output.
    #[arg(long = "output", value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Print paths relative to the current directory.
    #[arg(long = "relative")]
    pub relative: bool,

    /// Show the object names in full.
    #[arg(long = "no-abbrev")]
    pub no_abbrev: bool,

    /// Print a word-diff instead of a line diff. Not implemented in v1 (plan §8).
    #[arg(long = "word-diff", value_name = "MODE")]
    pub word_diff: Option<Option<String>>,
}

/// Phase 4 — walk and print commit history.
///
/// # Purpose
///
/// Start from a revision (or every ref, with `--all`), walk the ancestry, and print each commit
/// in the chosen order and format — the header alone by default, or a diff with `-p`.
///
/// # Exit codes
///
/// * `0` — the walk completed and the history was printed. An empty history is a success.
/// * `128` — the revision or range does not resolve, a pathspec matches nothing that is
///   tracked, or the object database could not be read.
///
/// # Caveats
///
/// * **The walk must terminate `--all` on a repository with a cycle-free graph but a *large*
///   one.** The visited set is the only thing preventing an unbounded walk on a history that
///   was rewritten to point at itself; see `rivet_core::WalkOptions::exclude` for the
///   termination conditions the core exposes.
/// * **The output format is compared to `git log` byte for byte** in the differential tests, so
///   the date format, the indentation of the message body and the blank line between entries
///   are all interface, not styling.
/// * **A commit's message may not be UTF-8.** The `encoding` header exists for that reason, and
///   writing the raw bytes to a terminal is what Git does; a handler that lossily decoded here
///   would print replacement characters where Git prints the original bytes.
/// * **`--graph` and `--decorate` are display-only and must not change the walk.** Computing the
///   graph by analysing the printed output rather than the commit graph produces wrong lanes at
///   every merge, and a walk whose order changed because `--graph` was passed is a bug that
///   shows up as two different `git log` outputs from the same repository.
/// * **An unborn `HEAD` is not an error for a bare `rivet log`; it prints nothing.** Git exits
///   `128` with `fatal: your current branch 'main' does not have any commits yet`, and that is
///   the behaviour a script sees, so the empty-history case must be distinguished from the
///   `--all`-with-no-refs case rather than both returning early.
pub fn run_log(global: &GlobalArgs, args: &LogArgs) -> Result<i32> {
    todo!()
}

/// Phase 4 — print one object in its human-readable form.
///
/// # Purpose
///
/// Resolve a revision, tag, tree, blob or `REV:path` and print it: a commit with its header and
/// diff, a tag with the object it peels to, a tree as a listing, a blob as raw bytes.
///
/// # Exit codes
///
/// * `0` — the object was found and printed.
/// * `128` — the revision does not resolve, the path is not in the tree, or the object is
///   missing from the database.
///
/// # Caveats
///
/// * **A blob is written to stdout byte for byte.** `rivet show HEAD:file > out` must produce a
///   file that is identical to the one committed — no added newline, no line-ending conversion,
///   no encoding. This is the one command where the output is not text.
/// * **The four kinds produce four different shapes**, per [`ShowArgs`]. A handler that branches
///   on the kind once, up front, and then dispatches is the only structure that keeps them from
///   bleeding into one another.
/// * **An annotated tag prints both the tag and its target**, and the tag's own object is
///   printed first. Stopping at the tag object looks like `show` failed to find the commit.
/// * **A merge commit's diff is against its first parent by default**, and against nothing at
///   all with a "combined diff" request, which is not implemented in v1. The default is what
///   `git show` prints and must not be the combined form.
/// * **`--format` and `--pretty` take a format string**, which is a small language (`%H`, `%s`,
///   `%an`, `%ad`, `%n`, `%%`) and not a name. An unknown directive is not a formatting error to
///   be swallowed; Git leaves it verbatim and so must Rivet, or a script's output changes shape.
pub fn run_show(global: &GlobalArgs, args: &ShowArgs) -> Result<i32> {
    todo!()
}

/// Phase 8 — show changes between two states.
///
/// # Purpose
///
/// Compute and print the difference between two of the four states: two commits, the index and
/// `HEAD`, the work tree and the index, or the work tree and a commit. The default, like Git's,
/// is the work tree against the index.
///
/// # Exit codes
///
/// * `0` — the difference was printed (or found, under `--quiet`). **Differences are not an
///   error by default.**
/// * `1` — `--quiet` or `--exit-code` was given and there were differences. The requested
///   negative, and the one case where a non-zero exit is the intended answer.
/// * `128` — a revision does not resolve, three or more positionals were given, or the output
///   file could not be created.
///
/// # Caveats
///
/// * **The four comparison cases are decided by the positional count and `--cached`**, per
///   [`DiffArgs`]. Anything that is not one of the four is a usage error, not a guess.
/// * **A mode change with no content change is still a difference.** `chmod +x` produces a
///   `old mode 100644 / new mode 100755` header and no hunks; a tree walk that compared blob
///   ids only would report the file as unmodified, because the *mode* is what changed.
/// * **Rename detection is not implemented in v1** (plan §8). A rename must therefore appear as
///   a deletion and an addition, and `-M` must fail loudly rather than being accepted and
///   ignored — an accepted-and-ignored `-M` makes a caller's threshold silently meaningless.
/// * **The binary check is a NUL byte in the first 8 KB**, as Git does, and it is a heuristic
///   that `--text` overrides. A file that is binary by that test but text by the user's
///   judgement must be diffable with `-a`, and one that is text by the test but contains a NUL
///   past 8 KB is diffed as text by Git too — matching that boundary matters more than being
///   right about it.
/// * **`--stat` output must agree with the hunks it summarises.** The added and deleted counts
///   come from the same parsed hunks the unified format prints, not from a separate line count,
///   or a `--stat` and a `-p` for the same pair disagree on the file's totals.
/// * **`--no-index` mode, comparing two paths outside a repository, is not implemented.** It is
///   how `git diff` is used as a plain diff tool, and a caller who runs `rivet diff a.txt b.txt`
///   outside a repository gets `NotARepository` rather than a diff of the two files.
pub fn run_diff(global: &GlobalArgs, args: &DiffArgs) -> Result<i32> {
    todo!()
}
