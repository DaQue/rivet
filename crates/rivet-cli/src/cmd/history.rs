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
    /// The revisions, ranges or refs to start from. Defaults to `HEAD`.
    #[arg(value_name = "REVISION")]
    pub revisions: Vec<String>,

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
    #[arg(long = "decorate", value_name = "WHEN", require_equals = true, num_args = 0..=1)]
    pub decorate: Option<Option<String>>,

    /// Do not print ref names beside commits.
    #[arg(long = "no-decorate")]
    pub no_decorate: bool,

    /// Print the commit graph's graph, as `git log --graph` does.
    #[arg(long = "graph")]
    pub graph: bool,

    /// Paint the output: `always`, `never` or `auto` (a bare `--color` means `always`).
    #[arg(long = "color", value_name = "WHEN", require_equals = true, num_args = 0..=1, overrides_with = "no_color")]
    pub color: Option<Option<String>>,

    /// Do not paint the output.
    #[arg(long = "no-color", overrides_with = "color")]
    pub no_color: bool,
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

    /// Paint the output: `always`, `never` or `auto` (a bare `--color` means `always`).
    #[arg(long = "color", value_name = "WHEN", require_equals = true, num_args = 0..=1, overrides_with = "no_color")]
    pub color: Option<Option<String>>,

    /// Do not paint the output.
    #[arg(long = "no-color", overrides_with = "color")]
    pub no_color: bool,
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

    /// Paint the patch: `always`, `never` or `auto` (a bare `--color` means `always`).
    #[arg(long = "color", value_name = "WHEN", require_equals = true, num_args = 0..=1, overrides_with = "no_color")]
    pub color: Option<Option<String>>,

    /// Do not paint the patch.
    #[arg(long = "no-color", overrides_with = "color")]
    pub no_color: bool,

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
    if args.graph && args.reverse {
        anyhow::bail!("options '--graph' and '--reverse' cannot be used together");
    }
    if args.graph && !args.paths.is_empty() {
        anyhow::bail!("log: --graph with a path limit is not implemented");
    }

    // Revisions: any number of `A..B`, `A...B`, `^A` and plain revisions; `--all` adds every ref.
    let mut starts: Vec<ObjectId> = Vec::new();
    let mut options = WalkOptions::new();
    let given: Vec<&str> = args.revisions.iter().map(String::as_str).collect();
    let specs = if given.is_empty() && !args.all {
        vec!["HEAD"]
    } else {
        given
    };

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
    }
    for spec in &specs {
        add_revision(&repository, spec, &mut starts, &mut options)?;
    }

    options.first_parent(args.first_parent);
    if let Some(n) = args.max_count {
        options.max_count(n);
    }
    if let Some(n) = args.skip {
        options.skip(n);
    }
    if args.topo_order || (args.graph && !args.author_date_order) {
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
    // The last of `--oneline`, `--pretty` and `--format` on the command line wins, as in git.
    let layout = match last_layout_option(&std::env::args().collect::<Vec<_>>()) {
        Some(Some(spec)) => pretty::Layout::parse(&spec)?,
        Some(None) => pretty::Layout::Oneline,
        None => match (&args.format, &args.pretty) {
            (Some(spec), _) | (None, Some(spec)) => pretty::Layout::parse(spec)?,
            _ if args.oneline => pretty::Layout::Oneline,
            _ => pretty::Layout::Medium,
        },
    };
    // `--oneline` abbreviates ids whatever layout follows it; `--pretty=oneline` prints them in full.
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
    // `--decorate[=short|full|auto|no]`; with no tty to ask, `auto` means no.
    let (decorate, decorate_full) = match args.decorate.as_ref().map(|d| d.as_deref()) {
        None | Some(Some("no")) | Some(Some("auto")) => (false, false),
        Some(None) | Some(Some("short")) => (true, false),
        Some(Some("full")) => (true, true),
        Some(Some(other)) => anyhow::bail!("invalid --decorate option: {other}"),
    };
    let decorations = if decorate && !args.no_decorate || layout_uses_decorations(&layout) {
        decoration_map(&repository, decorate_full)?
    } else {
        Default::default()
    };
    let palette = match log_color(&repository, args)? {
        Ok(false) => None,
        Ok(true) => {
            if let Some(DiffMode::Stat | DiffMode::Patch) = diff_mode {
                check_default_diff_colors(&repository)?;
            }
            Some(log_palette(&repository)?)
        }
        Err(code) => return Ok(code),
    };
    let diff_mode = match diff_mode {
        Some(DiffMode::Patch) if palette.is_some() => Some(DiffMode::ColorPatch),
        Some(DiffMode::Stat) if palette.is_some() => Some(DiffMode::ColorStat),
        other => other,
    };
    let shifts = if args.graph {
        root_shifts(&repository, &oids, args.first_parent)?
    } else {
        Vec::new()
    };
    let mut graph = args
        .graph
        .then(|| super::graph::Graph::new(palette.is_some(), args.first_parent));
    let mut out = Vec::new();
    for (position, oid) in oids.iter().enumerate() {
        let commit = read_commit(&repository, oid)?;
        if let Some(graph) = graph.as_mut() {
            graph.update(*oid, &commit.parents, shifts[position]);
        }
        if position > 0 {
            let separated = match &layout {
                l if l.is_block() => true,
                pretty::Layout::Format {
                    terminator: false, ..
                } => true,
                _ => false,
            };
            if separated {
                if let Some(graph) = graph.as_mut() {
                    graph.padding_line(&mut out);
                }
                out.push(b'\n');
            }
        }
        let text = render_commit(
            &repository,
            &layout,
            oid,
            &commit,
            &Look {
                date,
                full_ids,
                abbrev_commit: args.oneline && !args.no_abbrev_commit,
                decorations: &decorations,
                palette: palette.as_ref(),
            },
        );
        match graph.as_mut() {
            Some(graph) => {
                graph.show_commit(&mut out);
                let terminator_row = push_with_graph(graph, &text, true, &mut out);
                if !graph.is_commit_finished() {
                    if !text.ends_with(b"\n") {
                        out.push(b'\n');
                    }
                    graph.show_remainder(&mut out);
                }
                if terminator_row {
                    graph.padding_line(&mut out);
                    out.push(b'\n');
                }
            }
            None => out.extend_from_slice(&text),
        }
        if let Some(mode) = diff_mode {
            let diff = commit_diff(
                &repository,
                &commit,
                &diff_options,
                &args.paths_as_specs(&repository)?,
            )?;
            if !diff.is_empty() && (commit.parents.len() < 2 || args.first_parent) {
                let mut chunk = Vec::new();
                if layout.blank_before_diff() {
                    chunk.push(b'\n');
                }
                write_diff(&repository, mode, &diff, &mut chunk)?;
                match graph.as_mut() {
                    Some(graph) => {
                        push_with_graph(graph, &chunk, false, &mut out);
                    }
                    None => out.extend_from_slice(&chunk),
                }
            }
        }
    }
    write_stdout(&out)?;
    Ok(0)
}

/// The text of `SQUASH_MSG`: every commit reachable from `theirs` but not from `ours`, newest
/// first, as `log` prints them, under Git's fixed heading.
pub(super) fn squash_message(
    repository: &Repository,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Vec<u8>> {
    let mut options = WalkOptions::new();
    options.exclude(ours);
    let oids = rivet_core::revwalk::walk_oids(repository.odb(), &[theirs], &options)?;
    let mut out = b"Squashed commit of the following:\n".to_vec();
    for oid in &oids {
        let commit = read_commit(repository, oid)?;
        out.push(b'\n');
        out.extend_from_slice(&render_commit(
            repository,
            &pretty::Layout::Medium,
            oid,
            &commit,
            &Look {
                date: "default",
                full_ids: false,
                abbrev_commit: false,
                decorations: &Default::default(),
                palette: None,
            },
        ));
    }
    Ok(out)
}

/// Append `text` to `out` with the graph's prefix in front of each line. The first line is left
/// bare when `first_is_open` (the commit line was already started by `show_commit`). Returns
/// whether git's line terminator needs a padding row of its own once the commit's remaining graph
/// rows are out: that is so when the message ends in a newline of its own (a format ending in
/// `%n`) and that empty last line used up a branch-joining row (post-merge or collapsing).
fn push_with_graph(
    graph: &mut super::graph::Graph,
    text: &[u8],
    first_is_open: bool,
    out: &mut Vec<u8>,
) -> bool {
    let mut terminator_row = false;
    let mut lines = text.split_inclusive(|&b| b == b'\n').enumerate().peekable();
    while let Some((n, line)) = lines.next() {
        let pending = !graph.is_commit_finished();
        if n > 0 || !first_is_open {
            graph.padding_line(out);
        }
        out.extend_from_slice(line);
        terminator_row = n > 0 && pending && line == b"\n" && lines.peek().is_none();
    }
    terminator_row
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
    .map_err(|error| match error {
        rivet_core::Error::Message(message) if message.starts_with("log for ") => {
            anyhow::anyhow!(message)
        }
        _ => anyhow::anyhow!(
            "ambiguous argument '{spec}': unknown revision or path not in the working tree."
        ),
    })?;

    let palette = match resolve_color(&repository, args.no_color, args.color.as_ref())? {
        Ok(false) => None,
        Ok(true) => {
            check_default_diff_colors(&repository)?;
            Some(log_palette(&repository)?)
        }
        Err(code) => return Ok(code),
    };
    let mut out = Vec::new();
    show_object(&mut repository, &oid, args, palette.as_ref(), &mut out)?;
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

    let color = match resolve_color(&repository, args.no_color, args.color.as_ref())? {
        Ok(color) => color,
        Err(code) => return Ok(code),
    };
    if color {
        check_default_diff_colors(&repository)?;
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

    // Conflicted paths (work tree against the index only) are rendered the way git does: a
    // combined `diff --cc` when both sides are present, `* Unmerged path` plus the diff against
    // stage 2 otherwise.
    let mut unmerged_paths: Vec<bstr::BString> = if revisions.is_empty() {
        repository
            .index()
            .entries()
            .iter()
            .filter(|e| e.is_unmerged())
            .map(|e| e.path.clone())
            .collect()
    } else {
        Vec::new()
    };
    unmerged_paths.dedup();
    unmerged_paths.retain(|path| {
        specs.is_empty()
            || specs
                .iter()
                .any(|s| rivet_core::ops::pathspec_matches(BStr::new(&path[..]), BStr::new(&s[..])))
    });
    if args.stat && !unmerged_paths.is_empty() {
        anyhow::bail!("diff: --stat of conflicted paths is not implemented");
    }
    let mut changed = !diff.is_empty();
    if global.quiet {
        return Ok(if changed || !unmerged_paths.is_empty() {
            1
        } else {
            0
        });
    }
    let mut out = Vec::new();
    if unmerged_paths.is_empty() {
        if args.name_only {
            for file in &diff.files {
                out.extend_from_slice(&rivet_core::ops::quote_path(file.path(), false));
                out.push(b'\n');
            }
        } else if args.name_status {
            rivet_core::format_name_status(&diff, &mut out)?;
        } else if args.stat {
            rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut out)?;
            if color {
                out = colorize_stat(&out, &diff);
            }
        } else {
            rivet_core::format_unified(&diff, format, &mut out)?;
            if color {
                out = colorize_patch(&out)?;
            }
        }
    } else {
        if color {
            anyhow::bail!("diff: coloured output of unmerged paths is not implemented");
        }
        // One chunk of output per path, merged in path order with the ordinary changes.
        let mode = if args.name_only {
            DiffMode::NameOnly
        } else if args.name_status {
            DiffMode::NameStatus
        } else {
            DiffMode::Patch
        };
        let mut chunks: Vec<(bool, bstr::BString, Vec<u8>)> = Vec::new();
        for file in &diff.files {
            let mut one = rivet_core::TreeDiff::new();
            one.stats.push(file.stat());
            one.files.push(file.clone());
            let mut bytes = Vec::new();
            write_diff(&repository, mode, &one, &mut bytes)?;
            chunks.push((true, bstr::BString::from(file.path()), bytes));
        }
        for path in &unmerged_paths {
            let (bytes, combined) = unmerged_chunk(&repository, path, mode, args.cached, &options)?;
            changed |= !combined;
            chunks.push((!combined, path.clone(), bytes));
        }
        chunks.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        for (_, _, bytes) in chunks {
            out.extend_from_slice(&bytes);
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

/// How far right `log --graph` draws each commit. Git moves a root commit that is not shown last
/// off the column the next row would reuse (a run of root commits climbs one column per row), but
/// leaves it alone when the next row continues an existing column or when the root was reached
/// through a merge's second or later parent.
fn root_shifts(
    repository: &Repository,
    oids: &[ObjectId],
    first_parent: bool,
) -> Result<Vec<usize>> {
    let mut layout = super::graph::Graph::new(false, first_parent);
    let mut is_root = Vec::with_capacity(oids.len());
    let mut merge_parent = Vec::with_capacity(oids.len());
    let mut rows = Vec::with_capacity(oids.len());
    let mut merge_parents = std::collections::HashSet::new();
    for oid in oids {
        let commit = read_commit(repository, oid)?;
        layout.update(*oid, &commit.parents, 0);
        rows.push(layout.row_layout());
        is_root.push(commit.parents.is_empty());
        merge_parent.push(merge_parents.contains(oid));
        if !first_parent {
            merge_parents.extend(commit.parents.iter().skip(1).copied());
        }
        // Drive the state machine to the end of the commit as the real pass will.
        let mut scratch = Vec::new();
        layout.show_commit(&mut scratch);
        layout.show_remainder(&mut scratch);
    }
    let last = oids.len().saturating_sub(1);
    let mut shifts = vec![0; oids.len()];
    // A root (one not reached through a merge) leaves a blank column behind; a run of roots that
    // each open a new column leaves one more for each root in the run.
    let mut ghosts_above = 0;
    for i in 0..last {
        let (base, _, has_column) = rows[i];
        let ghosts = if has_column { 0 } else { ghosts_above };
        let eligible = is_root[i] && !merge_parent[i];
        ghosts_above = match (eligible, has_column) {
            (false, _) => 0,
            (true, true) => 1,
            (true, false) => ghosts + 1,
        };
        if !eligible {
            continue;
        }
        let mut col = base + ghosts;
        // A root drawn above a row that reuses its column would read as that row's parent, unless
        // that row is itself a root about to be shifted away, or just carries on an existing column.
        let next = i + 1;
        let next_shifted_root = is_root[next] && next != last;
        let next_continues = rows[next].2;
        if ghosts == 0 && !next_shifted_root && !next_continues {
            col += 1;
        }
        shifts[i] = col - base;
    }
    Ok(shifts)
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

/// Which of `--oneline`, `--pretty=<f>` and `--format=<f>` comes last before any `--`: `Some(None)`
/// for `--oneline`, `Some(Some(f))` for a format, `None` when the command line has none of them.
fn last_layout_option(argv: &[String]) -> Option<Option<String>> {
    let mut last = None;
    let mut words = argv.iter().skip(1);
    while let Some(word) = words.next() {
        if word == "--" {
            break;
        }
        if word == "--oneline" {
            last = Some(None);
        } else if let Some(spec) = word
            .strip_prefix("--pretty=")
            .or_else(|| word.strip_prefix("--format="))
        {
            last = Some(Some(spec.to_string()));
        } else if (word == "--pretty" || word == "--format")
            && let Some(spec) = words.next()
        {
            last = Some(Some(spec.clone()));
        }
    }
    last
}

pub(super) fn abbrev(repository: &Repository, oid: &ObjectId, full: bool) -> String {
    let hex = oid.to_string();
    if full {
        return hex;
    }
    repository
        .odb()
        .abbreviate(oid, abbrev_minimum(repository, None))
        .unwrap_or(hex)
}

/// The shortest length an abbreviation may have: `requested` (`--abbrev=<n>`, `--short=<n>`) when
/// positive, else `core.abbrev`, else the automatic length for the repository's size.
/// `core.abbrev` is `auto`, a false boolean for "full ids", or a number; a value git rejects is
/// treated as `auto` here.
pub(super) fn abbrev_minimum(repository: &Repository, requested: Option<usize>) -> usize {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    thread_local! {
        static CONFIGURED: RefCell<HashMap<PathBuf, Option<usize>>> = RefCell::new(HashMap::new());
    }
    let hex_len = repository.format().hex_len();
    if let Some(n) = requested.filter(|n| *n > 0) {
        return n.clamp(4, hex_len);
    }
    let configured = CONFIGURED.with(|cache| {
        *cache
            .borrow_mut()
            .entry(repository.git_dir().to_path_buf())
            .or_insert_with(|| {
                let value = repository
                    .config_value(BStr::new(b"core.abbrev"))
                    .ok()
                    .flatten()?;
                let text = String::from_utf8_lossy(&value).trim().to_ascii_lowercase();
                match text.as_str() {
                    "auto" | "" => None,
                    "no" | "false" | "off" | "0" => Some(hex_len),
                    other => other
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (4..=hex_len).contains(n)),
                }
            })
    });
    configured.unwrap_or_else(|| repository.odb().default_abbrev_len())
}

/// How a commit's diff is printed after its header.
#[derive(Clone, Copy)]
enum DiffMode {
    Patch,
    /// [`DiffMode::Patch`] painted with git's default diff colours.
    ColorPatch,
    Stat,
    /// [`DiffMode::Stat`] painted with git's default diff colours.
    ColorStat,
    NameOnly,
    NameStatus,
}

/// `color.diff` and `color.diff.<slot>` change what [`colorize_patch`] would write; it only knows
/// git's defaults, so a repository that sets any of them is refused by name.
fn check_default_diff_colors(repository: &Repository) -> Result<()> {
    for key in [
        "color.diff",
        "color.diff.context",
        "color.diff.plain",
        "color.diff.meta",
        "color.diff.frag",
        "color.diff.func",
        "color.diff.old",
        "color.diff.new",
        "color.diff.whitespace",
    ] {
        if repository
            .config_value(BStr::new(key.as_bytes()))?
            .is_some()
        {
            anyhow::bail!("not implemented: {key} when painting a patch");
        }
    }
    Ok(())
}

/// Paints the plain patch `format_unified` wrote the way `git diff --color=always` does with the
/// default palette: bold file headers, cyan hunk headers, red removals, green additions, and a
/// red background on trailing whitespace of an added line. A space before a tab in the indent of
/// an added line is highlighted by git as well; that is refused rather than guessed.
fn colorize_stat(plain: &[u8], diff: &rivet_core::TreeDiff) -> Vec<u8> {
    const RESET: &[u8] = b"\x1b[m";
    const OLD: &[u8] = b"\x1b[31m";
    const NEW: &[u8] = b"\x1b[32m";
    let paint = |out: &mut Vec<u8>, colour: &[u8], text: &[u8]| {
        if !text.is_empty() {
            out.extend_from_slice(colour);
            out.extend_from_slice(text);
            out.extend_from_slice(RESET);
        }
    };
    let mut out = Vec::with_capacity(plain.len() * 2);
    // One line per file in `diff.stats` order, then the summary, which git leaves unpainted.
    for (index, line) in plain.split_inclusive(|&b| b == b'\n').enumerate() {
        let Some(stat) = diff.stats.get(index) else {
            out.extend_from_slice(line);
            continue;
        };
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        if stat.is_binary {
            // ` name | Bin <old> -> <new> bytes`
            let body = text.strip_suffix(b" bytes");
            let arrow = body.and_then(|b| b.windows(4).rposition(|w| w == b" -> "));
            if let (Some(body), Some(arrow)) = (body, arrow) {
                let old_at = body[..arrow]
                    .iter()
                    .rposition(|&b| !b.is_ascii_digit())
                    .map_or(0, |at| at + 1);
                out.extend_from_slice(&body[..old_at]);
                paint(&mut out, OLD, &body[old_at..arrow]);
                out.extend_from_slice(b" -> ");
                paint(&mut out, NEW, &body[arrow + 4..]);
                out.extend_from_slice(b" bytes");
            } else {
                out.extend_from_slice(text);
            }
        } else {
            let marks = text
                .iter()
                .rev()
                .take_while(|&&b| b == b'+' || b == b'-')
                .count();
            let split = text.len() - marks;
            let plus = text[split..].iter().take_while(|&&b| b == b'+').count();
            out.extend_from_slice(&text[..split]);
            paint(&mut out, NEW, &text[split..split + plus]);
            paint(&mut out, OLD, &text[split + plus..]);
        }
        if line.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out
}

fn colorize_patch(plain: &[u8]) -> Result<Vec<u8>> {
    const RESET: &[u8] = b"\x1b[m";
    const BOLD: &[u8] = b"\x1b[1m";
    const FRAG: &[u8] = b"\x1b[36m";
    const OLD: &[u8] = b"\x1b[31m";
    const NEW: &[u8] = b"\x1b[32m";
    const BAD: &[u8] = b"\x1b[41m";
    let mut out = Vec::with_capacity(plain.len() * 2);
    let mut in_hunk = false;
    for line in plain.split_inclusive(|&b| b == b'\n') {
        let (text, newline) = match line.strip_suffix(b"\n") {
            Some(text) => (text, &b"\n"[..]),
            None => (line, &b""[..]),
        };
        if text.starts_with(b"diff --git ") {
            in_hunk = false;
        } else if text.starts_with(b"@@ ") {
            in_hunk = true;
        }
        if !in_hunk {
            if text.starts_with(b"Binary files ") {
                out.extend_from_slice(text);
            } else {
                out.extend_from_slice(BOLD);
                out.extend_from_slice(text);
                out.extend_from_slice(RESET);
            }
        } else if text.starts_with(b"@@ ") {
            // The header proper is cyan; git writes the function context after an empty colour.
            let end = text[3..]
                .windows(2)
                .position(|w| w == b"@@")
                .map_or(text.len(), |at| at + 3 + 2);
            out.extend_from_slice(FRAG);
            out.extend_from_slice(&text[..end]);
            out.extend_from_slice(RESET);
            if end < text.len() {
                out.push(b' ');
                out.extend_from_slice(RESET);
                out.extend_from_slice(&text[end + 1..]);
                out.extend_from_slice(RESET);
            }
        } else if let Some(body) = text.strip_prefix(b"+") {
            let kept = body
                .iter()
                .rposition(|b| !matches!(b, b' ' | b'\t' | b'\r' | 0x0b | 0x0c))
                .map_or(0, |at| at + 1);
            let indent: Vec<u8> = body
                .iter()
                .copied()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .collect();
            if indent
                .iter()
                .position(|&b| b == b' ')
                .is_some_and(|space| indent[space..].contains(&b'\t'))
            {
                anyhow::bail!("not implemented: highlighting a space before a tab in a patch");
            }
            out.extend_from_slice(NEW);
            out.push(b'+');
            out.extend_from_slice(RESET);
            if kept > 0 {
                out.extend_from_slice(NEW);
                out.extend_from_slice(&body[..kept]);
                out.extend_from_slice(RESET);
            }
            if kept < body.len() {
                out.extend_from_slice(BAD);
                out.extend_from_slice(&body[kept..]);
                out.extend_from_slice(RESET);
            }
        } else if text.starts_with(b"-") {
            out.extend_from_slice(OLD);
            out.extend_from_slice(text);
            out.extend_from_slice(RESET);
        } else {
            out.extend_from_slice(text);
            out.extend_from_slice(RESET);
        }
        out.extend_from_slice(newline);
    }
    Ok(out)
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
        DiffMode::ColorStat => {
            let mut plain = Vec::new();
            rivet_core::format_stat_sized(diff, Some(repository.odb()), &mut plain)?;
            out.extend_from_slice(&colorize_stat(&plain, diff));
        }
        DiffMode::Patch => rivet_core::format_unified(diff, repository.format(), out)?,
        DiffMode::ColorPatch => {
            let mut plain = Vec::new();
            rivet_core::format_unified(diff, repository.format(), &mut plain)?;
            out.extend_from_slice(&colorize_patch(&plain)?);
        }
    }
    Ok(())
}

/// Everything `diff` prints for one unmerged path: git's combined `diff --cc` when both sides of
/// the conflict are present (patch output only), otherwise `* Unmerged path` followed by the
/// work tree's difference from stage 2.
///
/// The flag says whether the chunk is a combined diff: git prints those immediately and the rest
/// when it flushes its diff queue, and does not count them for `--exit-code`.
fn unmerged_chunk(
    repository: &Repository,
    path: &bstr::BString,
    mode: DiffMode,
    cached: bool,
    options: &rivet_core::DiffOptions,
) -> Result<(Vec<u8>, bool)> {
    use rivet_core::index::Stage;
    let index = repository.index();
    let stage = |s: Stage| {
        index
            .entry_at(BStr::new(&path[..]), s)
            .ok()
            .flatten()
            .cloned()
    };
    let (ours, theirs) = (stage(Stage::Ours), stage(Stage::Theirs));
    let worktree = repository.worktree()?;
    let data = match worktree.read_file(BStr::new(&path[..])) {
        _ if cached => None,
        Ok(data) => Some(data),
        Err(rivet_core::Error::IoAtPath { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    let quoted = rivet_core::ops::quote_path(&path[..], false);
    let mut out = Vec::new();
    if let (false, DiffMode::Patch, Some(ours), Some(theirs), Some(data)) =
        (cached, mode, &ours, &theirs, &data)
    {
        let wt_mode = worktree.stat(BStr::new(&path[..]))?.map(|s| s.mode);
        if wt_mode != Some(ours.mode) || ours.mode != theirs.mode {
            anyhow::bail!("diff: combined diff of a conflicted mode change is not implemented");
        }
        let read = |oid: &ObjectId| -> Result<Vec<u8>> {
            Ok(repository
                .odb()
                .read_typed(oid, rivet_core::ObjectKind::Blob)?
                .into_data())
        };
        let (a, b) = (read(&ours.oid)?, read(&theirs.oid)?);
        if [&a, &b, data]
            .iter()
            .any(|d| rivet_core::diff::is_binary(d))
        {
            anyhow::bail!("diff: combined diff of binary files is not implemented");
        }
        let hex = |oid: &ObjectId| {
            let full = oid.to_hex();
            String::from_utf8_lossy(&full[..7.min(full.len())]).into_owned()
        };
        out.extend_from_slice(b"diff --cc ");
        out.extend_from_slice(&quoted);
        out.extend_from_slice(
            format!(
                "\nindex {},{}..0000000\n--- a/",
                hex(&ours.oid),
                hex(&theirs.oid)
            )
            .as_bytes(),
        );
        out.extend_from_slice(&quoted);
        out.extend_from_slice(b"\n+++ b/");
        out.extend_from_slice(&quoted);
        out.push(b'\n');
        out.extend_from_slice(&rivet_core::diff::combined::combined_hunks(
            &[&a, &b],
            data,
            options.context,
        ));
        return Ok((out, true));
    }
    match mode {
        DiffMode::Patch => {
            out.extend_from_slice(b"* Unmerged path ");
            out.extend_from_slice(&quoted);
            out.push(b'\n');
        }
        DiffMode::NameOnly => {
            out.extend_from_slice(&quoted);
            out.push(b'\n');
        }
        DiffMode::NameStatus => {
            out.extend_from_slice(b"U\t");
            out.extend_from_slice(&quoted);
            out.push(b'\n');
        }
        DiffMode::Stat | DiffMode::ColorStat => {
            unreachable!("--stat of conflicted paths is refused earlier")
        }
        DiffMode::ColorPatch => unreachable!("coloured conflicted paths are refused earlier"),
    }
    if let (false, Some(ours)) = (cached, &ours) {
        let file = rivet_core::diff::entry_diff(
            worktree,
            ours,
            data.as_deref(),
            repository.odb(),
            options,
        )?;
        if file.old_oid != file.new_oid || file.old_mode != file.new_mode {
            let mut one = rivet_core::TreeDiff::new();
            one.stats.push(file.stat());
            one.files.push(file);
            write_diff(repository, mode, &one, &mut out)?;
        }
    }
    Ok((out, false))
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
    full: bool,
) -> Result<std::collections::HashMap<ObjectId, Vec<pretty::Decoration>>> {
    use pretty::Shade;
    let mut refs = repository.refs().list_all(repository.odb())?;
    refs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut map: std::collections::HashMap<ObjectId, Vec<pretty::Decoration>> = Default::default();
    let head = repository.head()?;
    for (name, oid) in refs.iter().rev() {
        let bytes = name.as_bytes();
        let shown = |short: bool| {
            if short && !full {
                name.short_name().to_string()
            } else {
                String::from_utf8_lossy(bytes).into_owned()
            }
        };
        let mut pieces = Vec::new();
        if bytes.starts_with(b"refs/tags/") {
            pieces.push((Shade::Tag, "tag: ".to_string()));
            pieces.push((Shade::Tag, shown(true)));
        } else if bytes.starts_with(b"refs/heads/") {
            pieces.push((Shade::Branch, shown(true)));
        } else if bytes.starts_with(b"refs/remotes/") {
            pieces.push((Shade::Remote, shown(true)));
        } else if bytes.starts_with(b"refs/stash") {
            pieces.push((Shade::Stash, shown(false)));
        } else {
            // Git decorates only branches, remote branches, tags and the stash.
            continue;
        }
        let is_head = matches!(head.branch(), Some(branch) if branch == name);
        if is_head {
            pieces.insert(0, (Shade::Commit, " -> ".to_string()));
            pieces.insert(0, (Shade::Head, "HEAD".to_string()));
        }
        let list = map.entry(*oid).or_default();
        let decoration = pretty::Decoration { pieces };
        if is_head {
            list.insert(0, decoration);
        } else {
            list.push(decoration);
        }
    }
    if let rivet_core::HeadState::Detached { oid } = head {
        let commit = repository
            .refs()
            .peel_oid(oid, repository.odb())
            .unwrap_or(oid);
        map.entry(commit).or_default().insert(
            0,
            pretty::Decoration {
                pieces: vec![(Shade::Head, "HEAD".to_string())],
            },
        );
    }
    Ok(map)
}

/// Whether `log` paints its output: `--color[=when]` / `--no-color` win over `color.ui`, which
/// says `always`, `never`, `auto` or a boolean (true meaning `auto`). `Err(status)` means a usage
/// error was already reported.
fn log_color(repository: &Repository, args: &LogArgs) -> Result<std::result::Result<bool, i32>> {
    resolve_color(repository, args.no_color, args.color.as_ref())
}

/// [`log_color`] for any command that has `--color[=when]` and `--no-color`.
fn resolve_color(
    repository: &Repository,
    no_color: bool,
    color: Option<&Option<String>>,
) -> Result<std::result::Result<bool, i32>> {
    use std::io::IsTerminal;
    let auto =
        || std::io::stdout().is_terminal() && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
    if no_color {
        return Ok(Ok(false));
    }
    match color.map(|when| when.as_deref()) {
        Some(None) | Some(Some("always")) => return Ok(Ok(true)),
        Some(Some("never")) => return Ok(Ok(false)),
        Some(Some("auto")) => return Ok(Ok(auto())),
        Some(Some(_)) => {
            eprintln!("error: option `color' expects \"always\", \"auto\", or \"never\"");
            return Ok(Err(129));
        }
        None => {}
    }
    let Some(value) = repository.config_value(BStr::new(b"color.ui"))? else {
        return Ok(Ok(auto()));
    };
    Ok(Ok(match &value[..] {
        b"always" => true,
        b"never" => false,
        b"auto" => auto(),
        other => match rivet_core::parse_bool(BStr::new(other)) {
            Ok(true) => auto(),
            Ok(false) => false,
            Err(_) => anyhow::bail!(
                "bad boolean config value '{}' for 'color.ui'",
                String::from_utf8_lossy(other)
            ),
        },
    }))
}

/// Git's colours for `log`, with `color.diff.commit` and `color.decorate.*` applied on top.
fn log_palette(repository: &Repository) -> Result<pretty::Palette> {
    let mut palette = pretty::Palette::git_default();
    let slots: [(&str, &mut String); 6] = [
        ("color.diff.commit", &mut palette.commit),
        ("color.decorate.head", &mut palette.head),
        ("color.decorate.branch", &mut palette.branch),
        ("color.decorate.remotebranch", &mut palette.remote),
        ("color.decorate.tag", &mut palette.tag),
        ("color.decorate.stash", &mut palette.stash),
    ];
    for (key, slot) in slots {
        if let Some(value) = repository.config_value(BStr::new(key.as_bytes()))? {
            let text = String::from_utf8_lossy(&value).into_owned();
            match pretty::parse_color_spec(&text) {
                Some(sequence) => *slot = sequence,
                None => anyhow::bail!("unable to parse '{key}': unsupported color '{text}'"),
            }
        }
    }
    Ok(palette)
}

/// How `log` and friends present each commit: the date style, whether ids are shown in full,
/// the ref names to decorate with, and the colours (`None` prints plain text).
struct Look<'a> {
    date: &'a str,
    full_ids: bool,
    abbrev_commit: bool,
    decorations: &'a std::collections::HashMap<ObjectId, Vec<pretty::Decoration>>,
    palette: Option<&'a pretty::Palette>,
}

fn render_commit(
    repository: &Repository,
    layout: &pretty::Layout,
    oid: &ObjectId,
    commit: &LogCommit,
    look: &Look<'_>,
) -> Vec<u8> {
    let abbreviate = |o: &ObjectId| abbrev(repository, o, false);
    let entry = pretty::Entry {
        oid: *oid,
        tree: commit.tree,
        parents: &commit.parents,
        author: &commit.author,
        committer: &commit.committer,
        message: &commit.message,
        decorations: look.decorations.get(oid).map_or(&[], Vec::as_slice),
    };
    let style = pretty::Style {
        date: look.date,
        now: pretty::now(),
        abbrev: &abbreviate,
        full_ids: look.full_ids,
        abbrev_commit: look.abbrev_commit,
        palette: look.palette,
    };
    let mut out = pretty::render(layout, &entry, &style);
    // An empty `tformat:` prints nothing at all, not even the terminator.
    if let pretty::Layout::Format {
        template,
        terminator: true,
    } = layout
        && !template.is_empty()
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

/// Add one revision argument of `log` to the walk: a start point, or an exclusion. A walk that is
/// given only exclusions (`log ^A`) has no start and prints nothing, as in Git.
fn add_revision(
    repository: &Repository,
    spec: &str,
    starts: &mut Vec<ObjectId>,
    options: &mut WalkOptions,
) -> Result<()> {
    let resolve = |text: &str| super::resolve_commit(repository, text);
    // Git names the whole argument, not the failing side, when a range endpoint is unknown.
    let ambiguous = || {
        anyhow::anyhow!(
            "ambiguous argument '{spec}': unknown revision or path not in the working tree.\n\
             Use '--' to separate paths from revisions, like this:\n\
             'git <command> [<revision>...] -- [<file>...]'"
        )
    };
    let resolve_range =
        |text: &str| resolve(text).map_err(|error| super::rev_error(error, ambiguous));
    let mut start = |oid: ObjectId| {
        if !starts.contains(&oid) {
            starts.push(oid);
        }
    };

    if let Some(negated) = spec.strip_prefix('^') {
        options.exclude(resolve(negated).map_err(|_| anyhow::anyhow!("bad revision '{spec}'"))?);
        return Ok(());
    }
    if let Some((left, right)) = spec.split_once("...") {
        let (left, right) = (
            resolve_range(if left.is_empty() { "HEAD" } else { left })?,
            resolve_range(if right.is_empty() { "HEAD" } else { right })?,
        );
        start(left);
        start(right);
        for base in rivet_core::merge_bases(repository.odb(), left, right).unwrap_or_default() {
            options.exclude(base);
        }
    } else if let Some((left, right)) = spec.split_once("..") {
        options.exclude(resolve_range(if left.is_empty() { "HEAD" } else { left })?);
        start(resolve_range(if right.is_empty() {
            "HEAD"
        } else {
            right
        })?);
    } else {
        // An unborn branch has an empty history, which `git log` reports as an error.
        match resolve(spec) {
            Ok(oid) => start(oid),
            Err(error) => {
                if spec == "HEAD" {
                    anyhow::bail!(
                        "your current branch '{}' does not have any commits yet",
                        super::head_reflog_label(repository)?
                    );
                }
                return Err(super::rev_error(error, ambiguous));
            }
        }
    }
    Ok(())
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
    palette: Option<&pretty::Palette>,
    out: &mut Vec<u8>,
) -> Result<()> {
    // A `tag` or `tree` heading is in the commit colour, like a `commit` line.
    let heading = |out: &mut Vec<u8>, text: String| match palette {
        Some(palette) => {
            out.extend_from_slice(palette.commit.as_bytes());
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[m\n");
        }
        None => {
            out.extend_from_slice(text.as_bytes());
            out.push(b'\n');
        }
    };
    let object = repository.odb().read(oid)?;
    match object.kind() {
        rivet_core::ObjectKind::Blob => out.extend_from_slice(object.data()),
        rivet_core::ObjectKind::Tree => {
            heading(out, format!("tree {}", args.object));
            out.push(b'\n');
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
            heading(out, format!("tag {}", tag.name));
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
            show_object(repository, &target, args, palette, out)?;
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
                decoration_map(repository, false)?
            } else {
                Default::default()
            };
            out.extend_from_slice(&render_commit(
                repository,
                &layout,
                oid,
                &commit,
                &Look {
                    date,
                    full_ids,
                    abbrev_commit: false,
                    decorations: &decorations,
                    palette,
                },
            ));
            let diff_mode = if args.no_patch {
                None
            } else if args.name_only {
                Some(DiffMode::NameOnly)
            } else if args.name_status {
                Some(DiffMode::NameStatus)
            } else if args.stat {
                Some(if palette.is_some() {
                    DiffMode::ColorStat
                } else {
                    DiffMode::Stat
                })
            } else if palette.is_some() {
                Some(DiffMode::ColorPatch)
            } else {
                Some(DiffMode::Patch)
            };
            // A merge shows no patch (the combined diff is not implemented), but its diffstat is
            // against the first parent, as in Git.
            if let Some(mode) = diff_mode
                && (commit.parents.len() < 2
                    || matches!(mode, DiffMode::Stat | DiffMode::ColorStat))
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
        // A sparse-checkout placeholder has no file; the index stands in for the work tree.
        if let Some(entry) = repository
            .index()
            .entry(path_ref)
            .filter(|entry| entry.skip_worktree())
        {
            let tree_side = map.get(&path).copied();
            if tree_side != Some((entry.mode, entry.oid)) {
                files.push(rivet_core::blob_diff(
                    repository.format(),
                    path_ref,
                    tree_side.map(|(mode, oid)| (oid, mode)),
                    Some((entry.oid, entry.mode)),
                    repository.odb(),
                    options,
                )?);
            }
            continue;
        }
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
