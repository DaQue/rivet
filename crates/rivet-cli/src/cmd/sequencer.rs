//! `cherry-pick` and `revert`: replay one commit's change (or its inverse) onto `HEAD`.
//!
//! A single commit is merged with the real three-way machinery (`base = parent`, `theirs =
//! commit` for a pick; `base = commit`, `theirs = parent` for a revert). A conflict stops with
//! Git's resolvable state — conflict stages in the index, markers in the work tree, `MERGE_MSG`
//! and `CHERRY_PICK_HEAD` / `REVERT_HEAD` — and `--continue` / `--abort` conclude it.
//!
//! A sequence of several commits is rehearsed in memory first. Only a sequence that applies
//! cleanly end to end is carried out; one that would stop on a conflict is refused before
//! anything is written, because Rivet keeps no `.git/sequencer` state to resume from.

use std::path::Path;

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::{HeadState, ObjectId, ObjectKind, Repository, Signature};

use crate::cli::GlobalArgs;

/// Arguments to `rivet cherry-pick`.
#[derive(Debug, clap::Args)]
pub struct CherryPickArgs {
    /// Keep the original message without starting an editor. Rivet never starts one.
    #[arg(long = "no-edit")]
    pub no_edit: bool,

    /// The commits to apply: revisions, or `A..B` ranges (oldest first).
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,

    /// Apply the changes to the index and work tree without committing.
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,

    /// Cancel the operation in progress and return to the pre-operation state.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Commit the resolved conflict and finish the operation.
    #[arg(long = "continue")]
    pub continue_pick: bool,
}

/// Arguments to `rivet revert`.
#[derive(Debug, clap::Args)]
pub struct RevertArgs {
    /// The commit to revert.
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,

    /// Apply the inverse change to the index and work tree without committing.
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,

    /// Use the generated message without starting an editor (the only mode Rivet supports; it is
    /// also the default when stdin or stdout is not a terminal, as in Git).
    #[arg(long = "no-edit")]
    pub no_edit: bool,

    /// Cancel the operation in progress and return to the pre-operation state.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Commit the resolved conflict and finish the operation.
    #[arg(long = "continue")]
    pub continue_revert: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Pick,
    Revert,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Pick => "cherry-pick",
            Op::Revert => "revert",
        }
    }

    fn head_file(self) -> &'static str {
        match self {
            Op::Pick => "CHERRY_PICK_HEAD",
            Op::Revert => "REVERT_HEAD",
        }
    }

    fn gerund(self) -> &'static str {
        match self {
            Op::Pick => "Cherry-picking",
            Op::Revert => "Reverting",
        }
    }
}

struct Settings {
    op: Op,
    no_commit: bool,
    quiet: bool,
    /// Concluding a stopped operation records `commit (cherry-pick)`, as `git commit` does.
    concluding: bool,
}

/// Handler for `rivet cherry-pick`.
///
/// # Exit codes
///
/// * `0` — applied (and committed unless `--no-commit`).
/// * `1` — the change conflicts, or would be empty; the repository is left in Git's resolvable
///   state (or untouched, for an empty result).
/// * `128` — a fatal error: nothing to continue, merge commits, a refused sequence.
pub fn run_cherry_pick(global: &GlobalArgs, args: &CherryPickArgs) -> Result<i32> {
    run(
        global,
        Op::Pick,
        &args.commits,
        args.no_commit,
        args.abort,
        args.continue_pick,
    )
}

/// Handler for `rivet revert`; same exit codes as [`run_cherry_pick`].
pub fn run_revert(global: &GlobalArgs, args: &RevertArgs) -> Result<i32> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if interactive && !args.no_edit && !args.abort && !args.continue_revert {
        anyhow::bail!("revert: starting an editor is not supported; pass --no-edit");
    }
    run(
        global,
        Op::Revert,
        &args.commits,
        args.no_commit,
        args.abort,
        args.continue_revert,
    )
}

fn failed(op: Op) -> i32 {
    eprintln!("fatal: {} failed", op.name());
    128
}

fn run(
    global: &GlobalArgs,
    op: Op,
    commits: &[String],
    no_commit: bool,
    abort: bool,
    continue_op: bool,
) -> Result<i32> {
    let mut repository = super::open(global)?;
    let git_dir = repository.git_dir().to_path_buf();
    let settings = Settings {
        op,
        no_commit,
        quiet: global.quiet,
        concluding: false,
    };
    if abort && continue_op {
        anyhow::bail!("--abort and --continue cannot be used together");
    }
    if abort {
        return do_abort(&mut repository, op);
    }
    if continue_op {
        return do_continue(&mut repository, &settings);
    }
    if commits.is_empty() {
        eprintln!("usage: git {} [<options>] <commit>...", op.name());
        return Ok(129);
    }

    if repository.index().has_conflicts() {
        eprintln!(
            "error: {} is not possible because you have unmerged files.",
            op.gerund()
        );
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        return Ok(failed(op));
    }
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("{}: cannot run on an unborn branch", op.name());
    };

    let picks = collect_commits(&repository, commits)?;
    for pick in &picks {
        if pick.parents.len() > 1 {
            eprintln!(
                "error: commit {} is a merge but no -m option was given.",
                pick.oid
            );
            return Ok(failed(op));
        }
        if op == Op::Revert && pick.parents.is_empty() {
            anyhow::bail!("revert: reverting a root commit is not supported");
        }
    }

    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(&repository, "merge.renames");

    if picks.len() == 1 {
        return apply_one(&mut repository, &settings, &picks[0], &options);
    }

    if git_dir.join("CHERRY_PICK_HEAD").exists() || git_dir.join("REVERT_HEAD").exists() {
        eprintln!("error: a cherry-pick or revert is already in progress");
        eprintln!(
            "hint: try \"git {} (--continue | --skip | --abort | --quit)\"",
            op.name()
        );
        return Ok(failed(op));
    }

    // Rehearse the whole sequence on trees; nothing is written to the work tree yet.
    let mut tree = repository.commit_tree(head)?;
    let head_tree = tree;
    let mut touched: Vec<BString> = Vec::new();
    for pick in &picks {
        let (base, theirs) = sides(op, pick);
        let base_tree = match base {
            Some(base) => Some(repository.commit_tree(base)?),
            None => None,
        };
        let theirs_tree = repository.commit_tree(theirs)?;
        let result =
            rivet_core::merge_trees(repository.odb(), base_tree, tree, theirs_tree, &options)?;
        let Some(next) = result.tree.filter(|_| result.clean) else {
            eprintln!(
                "error: could not {} {}... {}",
                verb(op),
                super::history::abbrev(&repository, &pick.oid, false),
                pick.subject
            );
            eprintln!(
                "fatal: the sequence stops on a conflict, and Rivet cannot resume a sequence; \
                 nothing was changed. {} the commits one at a time instead",
                op.gerund()
            );
            return Ok(1);
        };
        if next == tree {
            eprintln!(
                "error: {} {} would be empty; nothing was changed",
                op.name(),
                pick.oid
            );
            return Ok(1);
        }
        touched.extend(result.paths.iter().map(|p| p.path.clone()));
        tree = next;
    }
    touched.sort();
    touched.dedup();
    if let Err(error) = preflight(&repository, head_tree, &touched) {
        return super::branching::report_checkout_refusal(error, "merge")
            .map(|code| if code == 1 { failed(op) } else { code });
    }

    if no_commit {
        if let Err(error) = repository.checkout_tree(tree, false) {
            return super::branching::report_checkout_refusal(error, "merge");
        }
        return Ok(0);
    }
    for pick in &picks {
        let code = apply_one(&mut repository, &settings, pick, &options)?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

fn verb(op: Op) -> &'static str {
    match op {
        Op::Pick => "apply",
        Op::Revert => "revert",
    }
}

/// A commit to replay, with what the sequencer needs from it.
struct Pick {
    oid: ObjectId,
    parents: Vec<ObjectId>,
    author: Signature,
    message: Vec<u8>,
    subject: String,
}

fn read_pick(repository: &Repository, oid: ObjectId) -> Result<Pick> {
    let object = repository.odb().read_typed(&oid, ObjectKind::Commit)?;
    let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
    let message = commit.message.to_vec();
    let subject = message
        .lines()
        .find(|l| !l.iter().all(u8::is_ascii_whitespace))
        .map(|l| String::from_utf8_lossy(l).trim_end().to_string())
        .unwrap_or_default();
    Ok(Pick {
        oid,
        parents: commit.parents.clone(),
        author: commit.author.clone(),
        message,
        subject,
    })
}

fn resolve(repository: &Repository, spec: &str) -> Result<ObjectId> {
    super::resolve_commit(repository, spec).map_err(|_| anyhow::anyhow!("bad revision '{spec}'"))
}

/// Expand the command-line revisions (`A`, `A..B`) into commits, oldest first.
fn collect_commits(repository: &Repository, specs: &[String]) -> Result<Vec<Pick>> {
    let mut picks = Vec::new();
    for spec in specs {
        if let Some((left, right)) = spec.split_once("..")
            && !spec.contains("...")
        {
            let mut options = rivet_core::revwalk::WalkOptions::new();
            options.exclude(resolve(
                repository,
                if left.is_empty() { "HEAD" } else { left },
            )?);
            options.sort(rivet_core::revwalk::SortOrder::Topo);
            let start = resolve(repository, if right.is_empty() { "HEAD" } else { right })?;
            let mut oids = rivet_core::revwalk::walk_oids(repository.odb(), &[start], &options)?;
            oids.reverse();
            for oid in oids {
                picks.push(read_pick(repository, oid)?);
            }
        } else {
            let oid = resolve(repository, spec)?;
            picks.push(read_pick(repository, oid)?);
        }
    }
    if picks.is_empty() {
        anyhow::bail!("empty commit set passed");
    }
    Ok(picks)
}

/// The (base, theirs) commits of the three-way merge that replays `pick`.
fn sides(op: Op, pick: &Pick) -> (Option<ObjectId>, ObjectId) {
    match op {
        Op::Pick => (pick.parents.first().copied(), pick.oid),
        Op::Revert => (Some(pick.oid), pick.parents[0]),
    }
}

/// Refuse when a path the sequence touches has local changes or is shadowed by an untracked file.
fn preflight(
    repository: &Repository,
    head_tree: ObjectId,
    touched: &[BString],
) -> rivet_core::Result<()> {
    let head_map = rivet_core::ops::flatten_tree(repository.odb(), head_tree)?;
    let worktree = repository.worktree()?;
    let mut dirty = Vec::new();
    let mut untracked = Vec::new();
    for path in touched {
        let path_ref = BStr::new(&path[..]);
        let index_entry = repository.index().entry(path_ref);
        match (index_entry, head_map.get(path)) {
            (None, None) => {
                if worktree.stat(path_ref)?.is_some() {
                    untracked.push(path.clone());
                }
            }
            (Some(entry), Some((mode, oid))) if entry.oid == *oid && entry.mode == *mode => {
                if worktree.stat(path_ref)?.is_some() && worktree.hash_file(path_ref)? != entry.oid
                {
                    dirty.push(path.clone());
                }
            }
            _ => dirty.push(path.clone()),
        }
    }
    match (dirty.is_empty(), untracked.is_empty()) {
        (true, true) => Ok(()),
        (false, true) => Err(rivet_core::Error::DirtyWorktree { paths: dirty }),
        _ => Err(rivet_core::Error::MergeBlocked { dirty, untracked }),
    }
}

/// The message a revert commit gets.
fn revert_message(pick: &Pick) -> Vec<u8> {
    let subject = &pick.subject;
    let title = match subject
        .strip_prefix("Revert \"")
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some(inner) => format!("Reapply \"{inner}\""),
        None => format!("Revert \"{subject}\""),
    };
    format!("{title}\n\nThis reverts commit {}.\n", pick.oid).into_bytes()
}

fn apply_one(
    repository: &mut Repository,
    settings: &Settings,
    pick: &Pick,
    options: &rivet_core::MergeOptions,
) -> Result<i32> {
    let op = settings.op;
    let git_dir = repository.git_dir().to_path_buf();
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("{}: cannot run on an unborn branch", op.name());
    };
    let (base, theirs) = sides(op, pick);
    let short = super::history::abbrev(repository, &pick.oid, false);
    let described = format!("{short} ({})", pick.subject);
    let label = match op {
        Op::Pick => described.clone(),
        Op::Revert => format!("parent of {described}"),
    };
    let mut options = options.clone();
    options.labels = (BString::from("HEAD"), BString::from(label.as_str()));

    let result = match repository.merge_into_worktree(base, theirs, &options) {
        Ok(result) => result,
        Err(error) => {
            return super::branching::report_checkout_refusal(error, "merge")
                .map(|code| if code == 1 { failed(op) } else { code });
        }
    };
    let conflicted = super::branching::print_merge_report(&result, &label, &[]);

    let message = match op {
        Op::Pick => pick.message.clone(),
        Op::Revert => revert_message(pick),
    };

    if !conflicted.is_empty() {
        let mut merge_msg = message.clone();
        merge_msg.extend_from_slice(b"\n# Conflicts:\n");
        for path in &conflicted {
            merge_msg.extend_from_slice(b"#\t");
            merge_msg.extend_from_slice(path);
            merge_msg.push(b'\n');
        }
        write_file(&git_dir, "MERGE_MSG", &merge_msg)?;
        eprintln!(
            "error: could not {} {short}... {}",
            if op == Op::Pick { "apply" } else { "revert" },
            pick.subject
        );
        if settings.no_commit {
            eprintln!("hint: after resolving the conflicts, mark the corrected paths");
            eprintln!("hint: with 'git add <paths>' or 'git rm <paths>'");
        } else {
            write_file(
                &git_dir,
                op.head_file(),
                format!("{}\n", pick.oid).as_bytes(),
            )?;
            let name = op.name();
            eprintln!("hint: After resolving the conflicts, mark them with");
            eprintln!("hint: \"git add/rm <pathspec>\", then run");
            eprintln!("hint: \"git {name} --continue\".");
            eprintln!("hint: You can instead skip this commit with \"git {name} --skip\".");
            eprintln!("hint: To abort and get back to the state before \"git {name}\",");
            eprintln!("hint: run \"git {name} --abort\".");
        }
        eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
        return Ok(1);
    }

    if settings.no_commit {
        return Ok(0);
    }

    let tree = repository.write_tree()?;
    if repository.head_tree_oid()? == Some(tree) {
        eprintln!(
            "The previous {} is now empty, possibly due to conflict resolution.",
            op.name()
        );
        eprintln!("nothing was changed; the commit is not recorded");
        return Ok(1);
    }
    let author = match op {
        Op::Pick => pick.author.clone(),
        Op::Revert => repository.author_signature()?,
    };
    // `cherry-pick` and `revert` run `prepare-commit-msg` (source `message`) and `post-commit`,
    // but neither `pre-commit` nor `commit-msg`. A refusing hook leaves the picked change staged
    // with `MERGE_MSG` (and, for a pick, `CHERRY_PICK_HEAD`) for `--continue`.
    let Some(message) = message_hooks(repository, &message, "message", false)? else {
        write_file(&git_dir, "MERGE_MSG", &message)?;
        if op == Op::Pick {
            write_file(
                &git_dir,
                op.head_file(),
                format!("{}\n", pick.oid).as_bytes(),
            )?;
        }
        eprintln!("error: 'prepare-commit-msg' hook failed");
        return Ok(1);
    };
    commit_and_report(repository, settings, tree, head, author, &message)?;
    Ok(0)
}

/// Pass `message` through `COMMIT_EDITMSG` and the hooks that may rewrite it: `prepare-commit-msg`
/// with `source`, then `commit-msg` when `verify` is set. Returns the message the hooks left, or
/// `None` when one of them refused.
fn message_hooks(
    repository: &Repository,
    message: &[u8],
    source: &str,
    verify: bool,
) -> Result<Option<Vec<u8>>> {
    use super::snapshot::{commit_hook, shown_path};
    let editmsg = repository.git_dir().join("COMMIT_EDITMSG");
    write_file(repository.git_dir(), "COMMIT_EDITMSG", message)?;
    let shown = shown_path(repository, &editmsg);
    if commit_hook(
        repository,
        "prepare-commit-msg",
        &[shown.as_os_str(), source.as_ref()],
        None,
    )
    .is_some_and(|code| code != 0)
    {
        return Ok(None);
    }
    if verify
        && commit_hook(repository, "commit-msg", &[shown.as_os_str()], None)
            .is_some_and(|code| code != 0)
    {
        return Ok(None);
    }
    Ok(Some(std::fs::read(&editmsg)?))
}

fn commit_and_report(
    repository: &mut Repository,
    settings: &Settings,
    tree: ObjectId,
    parent: ObjectId,
    author: Signature,
    message: &[u8],
) -> Result<()> {
    let committer = repository.committer_signature()?;
    let mut builder = rivet_core::CommitBuilder::new();
    builder.tree(tree);
    builder.parent(parent);
    builder.author(author.clone());
    builder.committer(committer.clone());
    builder.message(BStr::new(message));
    let body = builder.build(repository.format())?;
    let oid = repository.odb().write_raw(ObjectKind::Commit, &body)?;
    let subject = message.lines().next().unwrap_or_default();
    let prefix = if settings.concluding && settings.op == Op::Pick {
        "commit (cherry-pick)".to_string()
    } else if settings.concluding {
        "commit".to_string()
    } else {
        settings.op.name().to_string()
    };
    let reason = format!("{prefix}: {}", String::from_utf8_lossy(subject));
    repository.move_head(oid, BStr::new(reason.as_bytes()))?;
    super::snapshot::commit_hook(repository, "post-commit", &[], None);

    if settings.quiet {
        return Ok(());
    }
    let label = match repository.head()? {
        HeadState::Attached { branch, .. } | HeadState::Unborn { branch } => {
            branch.short_name().to_string()
        }
        HeadState::Detached { .. } => "detached HEAD".to_string(),
    };
    println!(
        "[{label} {}] {}",
        super::short_oid(&oid),
        super::commit_subject(repository, &oid)?
    );
    if author.name != committer.name || author.email != committer.email {
        println!(" Author: {} <{}>", author.name, author.email);
    }
    if !(settings.concluding && settings.op == Op::Revert) {
        println!(
            " Date: {}",
            rivet_core::ops::format_git_date(author.when, author.tz_offset_minutes)
        );
    }
    let old_tree = Some(repository.commit_tree(parent)?);
    super::print_shortstat_summary(repository, old_tree, tree)?;
    Ok(())
}

fn write_file(git_dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let path = git_dir.join(name);
    std::fs::write(&path, data)
        .map_err(|e| anyhow::anyhow!("could not write '{}': {e}", path.display()))
}

/// Which operation is stopped, and the commit it was replaying.
fn in_progress(repository: &Repository) -> Result<Option<(Op, ObjectId)>> {
    for op in [Op::Pick, Op::Revert] {
        let path = repository.git_dir().join(op.head_file());
        if let Ok(text) = std::fs::read_to_string(&path) {
            let oid = ObjectId::from_hex(repository.format(), BStr::new(text.trim().as_bytes()))?;
            return Ok(Some((op, oid)));
        }
    }
    Ok(None)
}

fn clear_state(git_dir: &Path) {
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_MSG", "AUTO_MERGE"] {
        let _ = std::fs::remove_file(git_dir.join(name));
    }
}

fn do_abort(repository: &mut Repository, op: Op) -> Result<i32> {
    let git_dir = repository.git_dir().to_path_buf();
    if in_progress(repository)?.is_none() {
        eprintln!("error: no cherry-pick or revert in progress");
        return Ok(failed(op));
    }
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("cannot abort on an unborn branch");
    };
    let worktree = repository.worktree()?.clone();
    let mut index = std::mem::take(repository.index_mut());
    let result = rivet_core::merge::restore_to_commit(
        &worktree,
        &mut index,
        repository.odb(),
        &git_dir,
        head,
    );
    *repository.index_mut() = index;
    result?;
    rivet_core::write_orig_head(&git_dir, head)?;
    let who = repository.committer_signature()?;
    let reason = format!("reset: moving to {head}");
    let entry = rivet_core::ReflogEntry::new(head, head, who, BString::from(reason));
    repository
        .refs()
        .append_reflog(&rivet_core::RefName::head(), &entry)?;
    clear_state(&git_dir);
    Ok(0)
}

fn do_continue(repository: &mut Repository, settings: &Settings) -> Result<i32> {
    let git_dir = repository.git_dir().to_path_buf();
    let Some((op, picked)) = in_progress(repository)? else {
        eprintln!("error: no cherry-pick or revert in progress");
        return Ok(failed(settings.op));
    };
    let settings = Settings {
        op,
        no_commit: false,
        quiet: settings.quiet,
        concluding: true,
    };
    if repository.index().has_conflicts() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in repository.index().entries() {
            if entry.is_unmerged() && seen.insert(entry.path.clone()) {
                println!("U\t{}", entry.path);
            }
        }
        eprintln!("error: Committing is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        eprintln!("fatal: Exiting because of an unresolved conflict.");
        return Ok(128);
    }
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("cannot continue on an unborn branch");
    };
    // This is `git commit`: `pre-commit` first, then the message hooks, whatever `--no-verify`.
    if super::snapshot::commit_hook(repository, "pre-commit", &[], None)
        .is_some_and(|code| code != 0)
    {
        return Ok(1);
    }
    let pick = read_pick(repository, picked)?;
    let saved = std::fs::read(git_dir.join("MERGE_MSG")).unwrap_or_default();
    let mut raw = saved.clone();
    if super::branching::strip_comment_lines(&saved)
        .iter()
        .all(u8::is_ascii_whitespace)
    {
        raw = match op {
            Op::Pick => pick.message.clone(),
            Op::Revert => revert_message(&pick),
        };
    }
    let source = if saved.is_empty() { "message" } else { "merge" };
    let Some(edited) = message_hooks(repository, &raw, source, true)? else {
        return Ok(1);
    };
    let message = super::branching::strip_comment_lines(&edited);
    let tree = repository.write_tree()?;
    if repository.head_tree_oid()? == Some(tree) {
        eprintln!(
            "The previous {} is now empty, possibly due to conflict resolution.",
            op.name()
        );
        eprintln!(
            "nothing to commit; use 'git {} --abort' to give up",
            op.name()
        );
        return Ok(1);
    }
    let author = match op {
        Op::Pick => pick.author.clone(),
        Op::Revert => repository.author_signature()?,
    };
    commit_and_report(repository, &settings, tree, head, author, &message)?;
    clear_state(&git_dir);
    Ok(0)
}
