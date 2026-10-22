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

use super::pretty;
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

    /// Print only the names of the changed paths after each commit.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Lay each commit out as `oneline`, `short`, `medium`, `full`, `fuller` or `format:<fmt>`.
    #[arg(long = "pretty", value_name = "FORMAT")]
    pub pretty: Option<String>,

    /// Lay each commit out with a format string (`tformat:` semantics).
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// Date style: `default`, `iso`, `iso-strict`, `relative`, `short`, `raw`, `unix`, `rfc2822`.
    #[arg(long = "date", value_name = "STYLE")]
    pub date: Option<String>,

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

    /// Print a diffstat instead of the patch.
    #[arg(long = "stat")]
    pub stat: bool,

    /// Print only the names of the changed paths instead of the patch.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Print the changed paths with a status letter instead of the patch.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Date style: `default`, `iso`, `iso-strict`, `relative`, `short`, `raw`, `unix`, `rfc2822`.
    #[arg(long = "date", value_name = "STYLE")]
    pub date: Option<String>,
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
    let repository = super::open(global)?;
    if args.graph {
        anyhow::bail!("log: --graph is not implemented");
    }
    let format = repository.format();

    // Revisions: `A..B`, `A...B`, `^A`, or a single revision; `--all` adds every ref.
    let mut starts: Vec<ObjectId> = Vec::new();
    let mut options = WalkOptions::new();
    let spec = args.revision.as_str();
    let range = rivet_core::parse_rev_spec(format, BStr::new(spec.as_bytes()), repository.refs());
    let resolve = |text: &str| super::resolve_commit(&repository, text);
    if args.all {
        for (_, oid) in repository.refs().list_all(repository.odb())? {
            if let Ok(commit) = repository.refs().peel_oid(oid, repository.odb())
                && !starts.contains(&commit)
            {
                starts.push(commit);
            }
        }
        if let Some(head) = repository.head()?.oid()
            && !starts.contains(&head)
        {
            starts.insert(0, head);
        }
    } else if let Some(negated) = spec.strip_prefix('^') {
        options.exclude(resolve(negated)?);
        starts.push(resolve("HEAD")?);
    } else if spec.contains("...") {
        let (left, right) = spec.split_once("...").expect("checked");
        let (left, right) = (
            resolve(if left.is_empty() { "HEAD" } else { left })?,
            resolve(if right.is_empty() { "HEAD" } else { right })?,
        );
        starts.push(left);
        starts.push(right);
        for base in rivet_core::merge_bases(repository.odb(), left, right).unwrap_or_default() {
            options.exclude(base);
        }
    } else if spec.contains("..") {
        let (left, right) = spec.split_once("..").expect("checked");
        let _ = &range;
        options.exclude(resolve(if left.is_empty() { "HEAD" } else { left })?);
        starts.push(resolve(if right.is_empty() { "HEAD" } else { right })?);
    } else {
        // An unborn branch has an empty history, which `git log` reports as an error.
        match super::resolve_commit(&repository, spec) {
            Ok(oid) => starts.push(oid),
            Err(error) => {
                if spec == "HEAD" {
                    anyhow::bail!(
                        "your current branch '{}' does not have any commits yet",
                        super::head_reflog_label(&repository)?
                    );
                }
                return Err(error);
            }
        }
    }

    options.first_parent(args.first_parent);
    if let Some(n) = args.max_count {
        options.max_count(n);
    }
    if let Some(n) = args.skip {
        options.skip(n);
    }
    if args.topo_order {
        options.sort(SortOrder::Topo);
    } else if args.author_date_order {
        options.sort(SortOrder::AuthorDate);
    } else if args.reverse {
        options.sort(SortOrder::Reverse);
    }
    if let Some(text) = &args.since {
        options.since = Some(
            rivet_core::ops::parse_approxidate(text)
                .ok_or_else(|| anyhow::anyhow!("invalid date format: {text}"))?,
        );
    }
    if let Some(text) = &args.until {
        options.until = Some(
            rivet_core::ops::parse_approxidate(text)
                .ok_or_else(|| anyhow::anyhow!("invalid date format: {text}"))?,
        );
    }
    for path in &args.paths {
        options
            .paths
            .push(super::worktree_path(&repository, path, "log")?);
    }

    let oids = rivet_core::revwalk::walk_oids(repository.odb(), &starts, &options)?;
    let diff_options = rivet_core::DiffOptions::new();
    let layout = match (&args.format, &args.pretty) {
        (Some(spec), _) | (None, Some(spec)) => pretty::Layout::parse(spec)?,
        _ if args.oneline => pretty::Layout::Oneline,
        _ => pretty::Layout::Medium,
    };
    // `--oneline` abbreviates ids; `--pretty=oneline` prints them in full.
    let full_ids = if layout == pretty::Layout::Oneline && !args.oneline {
        true
    } else {
        args.no_abbrev_commit
    };
    let date = args.date.as_deref().unwrap_or("default");
    pretty::check_date_style(date)?;
    let diff_mode = if args.name_only {
        Some(DiffMode::NameOnly)
    } else if args.name_status {
        Some(DiffMode::NameStatus)
    } else if args.stat {
        Some(DiffMode::Stat)
    } else if args.patch {
        Some(DiffMode::Patch)
    } else {
        None
    };
    let decorations = if layout_uses_decorations(&layout) {
        decoration_map(&repository)?
    } else {
        Default::default()
    };
    let mut out = Vec::new();
    for (position, oid) in oids.iter().enumerate() {
        let commit = read_commit(&repository, oid)?;
        if position > 0 {
            match &layout {
                l if l.is_block() => out.push(b'\n'),
                pretty::Layout::Format {
                    terminator: false, ..
                } => out.push(b'\n'),
                _ => {}
            }
        }
        out.extend_from_slice(&render_commit(
            &repository,
            &layout,
            oid,
            &commit,
            date,
            full_ids,
            &decorations,
        ));
        if let Some(mode) = diff_mode {
            let diff = commit_diff(
                &repository,
                &commit,
                &diff_options,
                &args.paths_as_specs(&repository)?,
            )?;
            if !diff.is_empty() && commit.parents.len() < 2 {
                if layout.blank_before_diff() {
                    out.push(b'\n');
                }
                write_diff(&repository, mode, &diff, &mut out)?;
            }
        }
    }
    write_stdout(&out)?;
    Ok(0)
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
    let mut repository = super::open(global)?;
    let format = repository.format();
    let spec = args.object.as_str();
    let oid = rivet_core::revparse::resolve_path(
        format,
        repository.refs(),
        repository.odb(),
        BStr::new(spec.as_bytes()),
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "ambiguous argument '{spec}': unknown revision or path not in the working tree."
        )
    })?;

    let mut out = Vec::new();
    show_object(&mut repository, &oid, args, &mut out)?;
    write_stdout(&out)?;
    Ok(0)
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
    let repository = super::open(global)?;
    let format = repository.format();
    if args.revisions.len() > 2 {
        anyhow::bail!("diff: combined diffs of three or more revisions are not implemented");
    }
    if args.word_diff.is_some() || args.binary || args.relative {
        anyhow::bail!("diff: --word-diff, --binary and --relative are not implemented");
    }
    if args.ignore_all_space || args.ignore_space_change || args.ignore_space_at_eol {
        anyhow::bail!("diff: whitespace-ignoring options are not implemented");
    }

    let mut options = rivet_core::DiffOptions::new();
    if let Some(n) = args.context {
        options.context = n;
    }
    if let Some(algorithm) = args.algorithm {
        options.algorithm = algorithm.into();
    }
    options.detect_binary = !args.text;

    // `A B -- paths` versus `A -- paths`: revisions come first, anything else is a path.
    let mut revisions = args.revisions.clone();
    let mut pathspecs: Vec<PathBuf> = args.paths.clone();
    while let Some(last) = revisions.last() {
        if super::resolve_rev(&repository, last).is_ok() {
            break;
        }
        pathspecs.insert(0, PathBuf::from(revisions.pop().expect("non-empty")));
    }
    let mut specs = Vec::new();
    for path in &pathspecs {
        specs.push(super::worktree_path(&repository, path, "diff")?);
    }

    let mut diff = if revisions.len() == 2 || (revisions.len() == 1 && revisions[0].contains(".."))
    {
        let (left, right) = if revisions.len() == 2 {
            (revisions[0].clone(), revisions[1].clone())
        } else {
            let (l, r) = revisions[0].split_once("..").expect("checked");
            (l.to_string(), r.to_string())
        };
        let left = repository.commit_tree(super::resolve_rev(
            &repository,
            if left.is_empty() { "HEAD" } else { &left },
        )?)?;
        let right = repository.commit_tree(super::resolve_rev(
            &repository,
            if right.is_empty() { "HEAD" } else { &right },
        )?)?;
        rivet_core::tree_diff(format, left, right, repository.odb(), &options)?
    } else if args.cached {
        let tree = match revisions.first() {
            Some(rev) => Some(repository.commit_tree(super::resolve_rev(&repository, rev)?)?),
            None => repository
                .head_tree()
                .ok()
                .flatten()
                .map(|_| ())
                .and(repository.head_tree_oid()?),
        };
        rivet_core::index_diff(repository.index(), tree, repository.odb(), &options)?
    } else if let Some(rev) = revisions.first() {
        let tree = repository.commit_tree(super::resolve_rev(&repository, rev)?)?;
        tree_vs_worktree(&repository, tree, &options)?
    } else {
        let worktree = repository.worktree()?.clone();
        let mut cache = rivet_core::StatCache::new();
        rivet_core::work_tree_diff(
            &worktree,
            repository.index(),
            &mut cache,
            repository.odb(),
            &options,
        )?
    };
    super::detect_renames(&repository, &mut diff, &options)?;
    if !specs.is_empty() {
        let keep: Vec<bool> = diff
            .files
            .iter()
            .map(|f| {
                specs
                    .iter()
                    .any(|s| rivet_core::ops::pathspec_matches(f.path(), BStr::new(&s[..])))
            })
            .collect();
        let mut index = 0;
        diff.files.retain(|_| {
            index += 1;
            keep[index - 1]
        });
        let mut index = 0;
        diff.stats.retain(|_| {
            index += 1;
            keep[index - 1]
        });
    }

    // Conflicted paths need Git's combined diff (`diff --cc`), which is not implemented; naming
    // them keeps the omission visible instead of silent.
    let unmerged_paths: Vec<bstr::BString> = if revisions.is_empty() && !args.cached {
        repository
            .index()
            .entries()
            .iter()
            .filter(|e| {
                e.stage() == rivet_core::index::Stage::Ours
                    || (e.is_unmerged() && e.stage() == rivet_core::index::Stage::Base)
            })
            .map(|e| e.path.clone())
            .collect()
    } else {
        Vec::new()
    };
    let changed = !diff.is_empty();
    if global.quiet {
        return Ok(if changed { 1 } else { 0 });
    }
    let mut out = Vec::new();
    if args.name_only {
        for file in &diff.files {
            out.extend_from_slice(&rivet_core::ops::quote_path(file.path(), false));
            out.push(b'\n');
        }
    } else if args.name_status {
        rivet_core::format_name_status(&diff, &mut out)?;
    } else if args.stat {
        rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut out)?;
    } else {
        rivet_core::format_unified(&diff, format, &mut out)?;
    }
    if !args.name_only && !args.name_status && !args.stat {
        let mut seen = std::collections::BTreeSet::new();
        for path in &unmerged_paths {
            if seen.insert(path.clone()) {
                out.extend_from_slice(format!("* Unmerged path {path}\n").as_bytes());
            }
        }
    }
    if let Some(path) = &args.output {
        std::fs::write(path, &out)?;
    } else {
        write_stdout(&out)?;
    }
    Ok(if args.exit_code && changed { 1 } else { 0 })
}

use bstr::BStr;
use rivet_core::revwalk::{SortOrder, WalkOptions};
use rivet_core::{ObjectId, Repository};

fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|_| stdout.flush()) {
        // `rivet log | head` closes the pipe early; that is not an error.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

/// An owned copy of what a log entry needs from a commit.
struct LogCommit {
    parents: Vec<ObjectId>,
    tree: ObjectId,
    author: rivet_core::Signature,
    committer: rivet_core::Signature,
    message: Vec<u8>,
}

fn read_commit(repository: &Repository, oid: &ObjectId) -> Result<LogCommit> {
    let object = repository
        .odb()
        .read_typed(oid, rivet_core::ObjectKind::Commit)?;
    let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
    Ok(LogCommit {
        parents: commit.parents.clone(),
        tree: commit.tree,
        author: commit.author.clone(),
        committer: commit.committer.clone(),
        message: commit.message.to_vec(),
    })
}

pub(super) fn abbrev(repository: &Repository, oid: &ObjectId, full: bool) -> String {
    let hex = oid.to_string();
    if full {
        return hex;
    }
    // Seven digits unless that is ambiguous; extend until the prefix is unique.
    let mut length = 7;
    while length < hex.len() {
        let prefix = &hex[..length];
        let unique = repository
            .odb()
            .oids()
            .map(|all| {
                all.iter()
                    .filter(|o| o.to_string().starts_with(prefix))
                    .count()
                    <= 1
            })
            .unwrap_or(true);
        if unique {
            break;
        }
        length += 1;
    }
    hex[..length].to_string()
}

/// How a commit's diff is printed after its header.
#[derive(Clone, Copy)]
enum DiffMode {
    Patch,
    Stat,
    NameOnly,
    NameStatus,
}

fn write_diff(
    repository: &Repository,
    mode: DiffMode,
    diff: &rivet_core::TreeDiff,
    out: &mut Vec<u8>,
) -> Result<()> {
    match mode {
        DiffMode::NameOnly => {
            for file in &diff.files {
                out.extend_from_slice(&rivet_core::ops::quote_path(file.path(), false));
                out.push(b'\n');
            }
        }
        DiffMode::NameStatus => rivet_core::format_name_status(diff, out)?,
        DiffMode::Stat => rivet_core::format_stat_sized(diff, Some(repository.odb()), out)?,
        DiffMode::Patch => rivet_core::format_unified(diff, repository.format(), out)?,
    }
    Ok(())
}

fn layout_uses_decorations(layout: &pretty::Layout) -> bool {
    match layout {
        pretty::Layout::Format { template, .. } => {
            template.contains("%d") || template.contains("%D")
        }
        _ => false,
    }
}

/// Ref names pointing at each commit, in Git's display order: `HEAD -> branch` first, then the
/// refs in reverse name order (tags, remote branches, local branches).
fn decoration_map(
    repository: &Repository,
) -> Result<std::collections::HashMap<ObjectId, Vec<String>>> {
    let mut refs = repository.refs().list_all(repository.odb())?;
    refs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut map: std::collections::HashMap<ObjectId, Vec<String>> = Default::default();
    let head = repository.head()?;
    for (name, oid) in refs.iter().rev() {
        let bytes = name.as_bytes();
        let label = if bytes.starts_with(b"refs/tags/") {
            format!("tag: {}", name.short_name())
        } else if bytes.starts_with(b"refs/heads/") || bytes.starts_with(b"refs/remotes/") {
            name.short_name().to_string()
        } else {
            continue;
        };
        let label = match head.branch() {
            Some(branch) if branch == name => format!("HEAD -> {label}"),
            _ => label,
        };
        let list = map.entry(*oid).or_default();
        if label.starts_with("HEAD -> ") {
            list.insert(0, label);
        } else {
            list.push(label);
        }
    }
    if let rivet_core::HeadState::Detached { oid } = head {
        let commit = repository
            .refs()
            .peel_oid(oid, repository.odb())
            .unwrap_or(oid);
        map.entry(commit).or_default().insert(0, "HEAD".to_string());
    }
    Ok(map)
}

fn render_commit(
    repository: &Repository,
    layout: &pretty::Layout,
    oid: &ObjectId,
    commit: &LogCommit,
    date: &str,
    full_ids: bool,
    decorations: &std::collections::HashMap<ObjectId, Vec<String>>,
) -> Vec<u8> {
    let abbreviate = |o: &ObjectId| abbrev(repository, o, false);
    let entry = pretty::Entry {
        oid: *oid,
        tree: commit.tree,
        parents: &commit.parents,
        author: &commit.author,
        committer: &commit.committer,
        message: &commit.message,
        decorations: decorations.get(oid).map_or(&[], Vec::as_slice),
    };
    let style = pretty::Style {
        date,
        now: pretty::now(),
        abbrev: &abbreviate,
        full_ids,
    };
    let mut out = pretty::render(layout, &entry, &style);
    if let pretty::Layout::Format {
        terminator: true, ..
    } = layout
    {
        out.push(b'\n');
    }
    out
}

impl crate::cmd::history::LogArgs {
    fn paths_as_specs(&self, repository: &Repository) -> Result<Vec<bstr::BString>> {
        let mut specs = Vec::new();
        for path in &self.paths {
            specs.push(super::worktree_path(repository, path, "log")?);
        }
        Ok(specs)
    }
}

/// The diff a commit introduces against its first parent (or the empty tree).
fn commit_diff(
    repository: &Repository,
    commit: &LogCommit,
    options: &rivet_core::DiffOptions,
    specs: &[bstr::BString],
) -> Result<rivet_core::TreeDiff> {
    let base = match commit.parents.first() {
        Some(parent) => repository.commit_tree(*parent)?,
        None => super::empty_tree(repository)?,
    };
    let mut diff = rivet_core::tree_diff(
        repository.format(),
        base,
        commit.tree,
        repository.odb(),
        options,
    )?;
    super::detect_renames(repository, &mut diff, options)?;
    if !specs.is_empty() {
        let keep: Vec<bool> = diff
            .files
            .iter()
            .map(|f| {
                specs
                    .iter()
                    .any(|s| rivet_core::ops::pathspec_matches(f.path(), BStr::new(&s[..])))
            })
            .collect();
        let mut i = 0;
        diff.files.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        let mut i = 0;
        diff.stats.retain(|_| {
            i += 1;
            keep[i - 1]
        });
    }
    Ok(diff)
}

fn show_object(
    repository: &mut Repository,
    oid: &ObjectId,
    args: &ShowArgs,
    out: &mut Vec<u8>,
) -> Result<()> {
    let object = repository.odb().read(oid)?;
    match object.kind() {
        rivet_core::ObjectKind::Blob => out.extend_from_slice(object.data()),
        rivet_core::ObjectKind::Tree => {
            out.extend_from_slice(format!("tree {}\n\n", args.object).as_bytes());
            let tree = rivet_core::Tree::parse(repository.format(), object.data())?;
            for entry in &tree.entries {
                out.extend_from_slice(&entry.name);
                if entry.mode == rivet_core::EntryMode::Tree {
                    out.push(b'/');
                }
                out.push(b'\n');
            }
        }
        rivet_core::ObjectKind::Tag => {
            let tag = rivet_core::Tag::parse(repository.format(), object.data())?;
            out.extend_from_slice(format!("tag {}\n", tag.name).as_bytes());
            out.extend_from_slice(
                format!("Tagger: {} <{}>\n", tag.tagger.name, tag.tagger.email).as_bytes(),
            );
            out.extend_from_slice(
                format!(
                    "Date:   {}\n\n",
                    rivet_core::ops::format_git_date(tag.tagger.when, tag.tagger.tz_offset_minutes)
                )
                .as_bytes(),
            );
            out.extend_from_slice(tag.message);
            if !tag.message.ends_with(b"\n") {
                out.push(b'\n');
            }
            out.push(b'\n');
            let target = tag.object;
            show_object(repository, &target, args, out)?;
        }
        rivet_core::ObjectKind::Commit => {
            let commit = read_commit(repository, oid)?;
            let layout = match (&args.format, &args.pretty) {
                (Some(spec), _) | (None, Some(spec)) => pretty::Layout::parse(spec)?,
                _ => pretty::Layout::Medium,
            };
            let date = args.date.as_deref().unwrap_or("default");
            pretty::check_date_style(date)?;
            let full_ids = layout == pretty::Layout::Oneline;
            let decorations = if layout_uses_decorations(&layout) {
                decoration_map(repository)?
            } else {
                Default::default()
            };
            out.extend_from_slice(&render_commit(
                repository,
                &layout,
                oid,
                &commit,
                date,
                full_ids,
                &decorations,
            ));
            let diff_mode = if args.no_patch {
                None
            } else if args.name_only {
                Some(DiffMode::NameOnly)
            } else if args.name_status {
                Some(DiffMode::NameStatus)
            } else if args.stat {
                Some(DiffMode::Stat)
            } else {
                Some(DiffMode::Patch)
            };
            // A merge shows no patch (the combined diff is not implemented), but its diffstat is
            // against the first parent, as in Git.
            if let Some(mode) = diff_mode
                && (commit.parents.len() < 2 || matches!(mode, DiffMode::Stat))
            {
                let mut options = rivet_core::DiffOptions::new();
                if let Some(n) = args.context {
                    options.context = n;
                }
                let mut specs = Vec::new();
                for path in &args.paths {
                    specs.push(super::worktree_path(repository, path, "show")?);
                }
                let diff = commit_diff(repository, &commit, &options, &specs)?;
                if !diff.is_empty() {
                    if layout.blank_before_diff() {
                        out.push(b'\n');
                    }
                    write_diff(repository, mode, &diff, out)?;
                }
            }
        }
    }
    Ok(())
}

/// A tree compared with the work tree: every tracked path's current bytes against the tree's.
fn tree_vs_worktree(
    repository: &Repository,
    tree: ObjectId,
    options: &rivet_core::DiffOptions,
) -> Result<rivet_core::TreeDiff> {
    let worktree = repository.worktree()?;
    let map = rivet_core::ops::flatten_tree(repository.odb(), tree)?;
    let mut paths: std::collections::BTreeSet<bstr::BString> = map.keys().cloned().collect();
    for entry in repository.index().entries() {
        paths.insert(entry.path.clone());
    }
    let mut files = Vec::new();
    for path in paths {
        let path_ref = BStr::new(&path[..]);
        let data = match worktree.stat(path_ref)? {
            Some(_) => Some(worktree.read_file(path_ref)?),
            None => None,
        };
        match (map.get(&path), data) {
            (Some((mode, oid)), data) => {
                let entry = rivet_core::ops::make_index_entry(path_ref, *mode, *oid, None);
                let file = rivet_core::entry_diff(
                    worktree,
                    &entry,
                    data.as_deref(),
                    repository.odb(),
                    options,
                )?;
                if file.old_oid != file.new_oid || file.old_mode != file.new_mode {
                    files.push(file);
                }
            }
            (None, Some(_)) => {
                // Tracked in the index only: it is an addition relative to the tree.
                if let Some(entry) = repository.index().entry(path_ref) {
                    files.push(rivet_core::blob_diff(
                        repository.format(),
                        path_ref,
                        None,
                        Some((entry.oid, entry.mode)),
                        repository.odb(),
                        options,
                    )?);
                }
            }
            (None, None) => {}
        }
    }
    let stats = files.iter().map(rivet_core::FileDiff::stat).collect();
    Ok(rivet_core::TreeDiff { files, stats })
}
