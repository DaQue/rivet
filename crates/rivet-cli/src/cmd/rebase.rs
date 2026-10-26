//! `rebase`: replay the commits of the current branch on top of another base (the linear case).
//!
//! # What is implemented
//!
//! * `rebase <upstream> [<branch>]` and `--onto <newbase> <upstream>`: the commits of
//!   `<upstream>..<branch>` are replayed one at a time, merge commits are flattened away, and a
//!   commit whose patch already exists upstream is dropped, exactly as Git's merge backend does.
//! * `--continue`, `--skip` and `--abort` over a stopped rebase.
//! * `--interactive`, a usable subset: see [`interactive`].
//! * The state lives in `.git/rebase-merge/` with Git's file names (`head-name`, `onto`,
//!   `orig-head`, `done`, `git-rebase-todo`, `msgnum`, `end`, `stopped-sha`, `message`,
//!   `author-script`), so `status` can describe it.
//!
//! # What is refused
//!
//! `--root`, `--rebase-merges`, `--exec` without `-i`, `--autosquash`, `--edit-todo` and signing
//! are accepted by the grammar only to be refused by name; the other options Git has are not in the
//! grammar, so they fail at the command line instead of being ignored. What `--interactive` refuses
//! is listed in [`interactive`].
//!
//! # Caveats
//!
//! * **A conflict stops loudly**: exit 1, `rebase-merge/` kept, the index holding the conflict
//!   stages, `REBASE_HEAD` and `MERGE_MSG` written. It never commits a conflicted tree.
//! * **Patch equivalence uses Rivet's own patch id** (a hash of the normalised hunks), not Git's
//!   number; it decides the same question, which commits are already upstream.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::ops::HeadTarget;
use rivet_core::revwalk::{SortOrder, WalkOptions};
use rivet_core::{HeadState, ObjectId, ObjectKind, RefName, ReflogEntry, Repository, Signature};

use crate::cli::GlobalArgs;

mod interactive;

/// git's usage text for `rebase`, printed on stderr when the positionals do not fit.
const REBASE_USAGE: &str = include_str!("rebase_usage.txt");

/// Arguments of `rebase`.
#[derive(Debug, Default, clap::Args)]
pub struct RebaseArgs {
    /// The upstream to rebase onto (default: the branch's configured upstream).
    #[arg(value_name = "UPSTREAM")]
    pub upstream: Option<String>,

    /// The branch to rebase (default: the current one); it is switched to first.
    #[arg(value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Positionals beyond the second; git answers them with its usage text.
    #[arg(value_name = "EXTRA", hide = true)]
    pub extra: Vec<String>,

    /// Start the new history at this commit instead of at the upstream.
    #[arg(long = "onto", value_name = "NEWBASE")]
    pub onto: Option<String>,

    /// Continue after resolving the conflicts of a stopped rebase.
    #[arg(long = "continue")]
    pub continue_rebase: bool,

    /// Give up and restore the branch to where it was.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Skip the commit that stopped the rebase and go on.
    #[arg(long = "skip")]
    pub skip: bool,

    /// Forget the rebase in progress without touching `HEAD` or the work tree.
    #[arg(long = "quit")]
    pub quit: bool,

    /// Show the patch of the commit the rebase stopped at.
    #[arg(long = "show-current-patch")]
    pub show_current_patch: bool,

    /// Stash local changes before the rebase and apply them afterwards.
    #[arg(long = "autostash")]
    pub autostash: bool,

    /// Do not stash local changes (overrides `rebase.autoStash`).
    #[arg(long = "no-autostash")]
    pub no_autostash: bool,

    /// Skip the `pre-rebase` hook.
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Let the todo list be edited before the commits are replayed.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Rebase every commit down to the root.
    #[arg(long = "root")]
    pub root: bool,

    /// Refused: recreate merge commits.
    #[arg(short = 'r', long = "rebase-merges", num_args = 0..=1, require_equals = true)]
    pub rebase_merges: Option<Option<String>>,

    /// Refused: run a command after each commit.
    #[arg(short = 'x', long = "exec", value_name = "CMD")]
    pub exec: Vec<String>,

    /// Refused: keep the merge base of the branch and its upstream.
    #[arg(long = "keep-base")]
    pub keep_base: bool,

    /// Refused: sign the rewritten commits.
    #[arg(short = 'S', long = "gpg-sign", num_args = 0..=1, require_equals = true)]
    pub gpg_sign: Option<Option<String>>,

    /// With `--interactive`: move `fixup!` and `squash!` commits next to their targets.
    #[arg(long = "autosquash", overrides_with = "no_autosquash")]
    pub autosquash: bool,

    /// Do not reorder `fixup!` and `squash!` commits (overrides `rebase.autoSquash`).
    #[arg(long = "no-autosquash", overrides_with = "autosquash")]
    pub no_autosquash: bool,

    /// Edit the remaining todo list of an interactive rebase.
    #[arg(long = "edit-todo")]
    pub edit_todo: bool,
}

/// What the reflog calls this command: `rebase`, or `pull --rebase` when `pull` drives it.
static ACTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Make the reflog messages of this run say `action` (Git's `GIT_REFLOG_ACTION`).
pub(super) fn set_action(action: &str) {
    let _ = ACTION.set(action.to_string());
}

fn action() -> &'static str {
    ACTION.get().map_or("rebase", String::as_str)
}

/// The `GIT_REFLOG_ACTION` Git exports to hooks: only `pull` sets one for its rebase.
fn hook_action() -> Option<&'static str> {
    Some(action()).filter(|a| a.starts_with("pull"))
}

/// Run a hook of the rebase with the environment Git gives it.
fn rebase_hook(
    repository: &Repository,
    name: &str,
    args: &[String],
    stdin: Option<Vec<u8>>,
    reflog_action: Option<&str>,
) -> Option<i32> {
    let mut hook = rivet_core::hooks::Hook::new(repository, name);
    for arg in args {
        hook = hook.arg(arg);
    }
    if let Some(data) = stdin {
        hook = hook.stdin(data);
    }
    if let Some(action) = reflog_action {
        hook = hook.env("GIT_REFLOG_ACTION", action);
    }
    hook.run()
}

fn rebase_dir(repository: &Repository) -> PathBuf {
    repository.git_dir().join("rebase-merge")
}

fn read_state(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string()
}

fn write_state(dir: &Path, name: &str, text: &str) -> Result<()> {
    std::fs::write(dir.join(name), text)
        .map_err(|e| anyhow::anyhow!("could not write '{}': {e}", dir.join(name).display()))
}

/// The hash two commits share when they make the same change: file names, modes and the changed
/// and context lines with all whitespace removed. Hunk positions do not take part.
pub(super) fn patch_id(repository: &Repository, oid: &ObjectId) -> Result<Vec<u8>> {
    let tree = repository.commit_tree(*oid)?;
    let parents = repository.commit_parents(*oid)?;
    let base = match parents.first() {
        Some(parent) => repository.commit_tree(*parent)?,
        None => super::empty_tree(repository)?,
    };
    let diff = rivet_core::tree_diff(
        repository.format(),
        base,
        tree,
        repository.odb(),
        &rivet_core::DiffOptions::new(),
    )?;
    let mut hasher = repository.format().hasher();
    for file in &diff.files {
        hasher.update(file.path());
        hasher.update(b"\0");
        let mode = |m: Option<rivet_core::object::EntryMode>| m.map(|m| m.to_u32()).unwrap_or(0);
        hasher.update(format!("{:o} {:o}\0", mode(file.old_mode), mode(file.new_mode)).as_bytes());
        if file.is_binary {
            hasher.update(
                format!(
                    "binary {:?} {:?}",
                    file.old_oid.map(|o| o.to_string()),
                    file.new_oid.map(|o| o.to_string())
                )
                .as_bytes(),
            );
        }
        for hunk in &file.hunks {
            for line in &hunk.lines {
                hasher.update(&[line.kind.prefix() as u8]);
                let squeezed: Vec<u8> = line
                    .content
                    .iter()
                    .copied()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect();
                hasher.update(&squeezed);
            }
        }
    }
    Ok(hasher.finish()?.as_bytes().to_vec())
}

pub(super) fn subject_of(repository: &Repository, oid: &ObjectId) -> Result<String> {
    let object = repository.odb().read_typed(oid, ObjectKind::Commit)?;
    let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
    Ok(commit
        .message
        .lines()
        .find(|l| !l.iter().all(u8::is_ascii_whitespace))
        .map(|l| String::from_utf8_lossy(l).trim_end().to_string())
        .unwrap_or_default())
}

struct Commit {
    author: Signature,
    message: Vec<u8>,
    parents: Vec<ObjectId>,
}

fn read_commit(repository: &Repository, oid: &ObjectId) -> Result<Commit> {
    let object = repository.odb().read_typed(oid, ObjectKind::Commit)?;
    let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
    Ok(Commit {
        author: commit.author.clone(),
        message: commit.message.to_vec(),
        parents: commit.parents.clone(),
    })
}

/// Handler for `rivet rebase`.
///
/// # Exit codes
///
/// * `0` — finished (or already up to date).
/// * `1` — stopped on a conflict, or refused because of local changes.
/// * `128` — fatal: unknown revision, a rebase already in progress, nothing to continue.
pub fn run_rebase(global: &GlobalArgs, args: &RebaseArgs) -> Result<i32> {
    if !args.extra.is_empty() {
        eprint!("{REBASE_USAGE}");
        return Ok(129);
    }
    let mut repository = super::open(global)?;
    let dir = rebase_dir(&repository);
    if args.keep_base {
        for (given, other) in [(args.onto.is_some(), "--onto"), (args.root, "--root")] {
            if given {
                eprintln!("fatal: options '--keep-base' and '{other}' cannot be used together");
                return Ok(128);
            }
        }
    }
    for (given, what) in [
        (args.rebase_merges.is_some(), "--rebase-merges"),
        (
            !args.exec.is_empty() && !args.interactive,
            "--exec without -i",
        ),
        (args.gpg_sign.is_some(), "--gpg-sign"),
        (
            args.autosquash && !args.interactive,
            "--autosquash without -i",
        ),
    ] {
        if given {
            anyhow::bail!("rebase {what} is not implemented");
        }
    }
    let controls = [
        args.continue_rebase,
        args.abort,
        args.skip,
        args.quit,
        args.show_current_patch,
        args.edit_todo,
    ]
    .iter()
    .filter(|on| **on)
    .count();
    if controls > 1 {
        anyhow::bail!(
            "options '--continue', '--abort', '--skip', '--quit' and '--show-current-patch' cannot be used together"
        );
    }
    if controls == 1 {
        if !dir.is_dir() {
            eprintln!("fatal: no rebase in progress");
            return Ok(128);
        }
        if args.edit_todo {
            return interactive::edit_todo(&repository, &dir);
        }
        if args.abort {
            return abort(&mut repository, &dir);
        }
        if args.quit {
            // The rebase is forgotten where it stands: git leaves `REBASE_HEAD`, `MERGE_MSG` and
            // `AUTO_MERGE` behind. An autostash is kept as a stash entry.
            let stash = saved_autostash(&repository, &dir);
            std::fs::remove_dir_all(&dir)?;
            if let Some(stash) = stash {
                super::stash::autostash_store(&repository, stash, "autostash")?;
                eprintln!(
                    "Autostash exists; creating a new stash entry.\nYour changes are safe in the stash.\nYou can run \"git stash pop\" or \"git stash drop\" at any time."
                );
            }
            return Ok(0);
        }
        if args.show_current_patch {
            drop(repository);
            return super::history::run_show(
                global,
                &super::history::ShowArgs {
                    object: "REBASE_HEAD".to_string(),
                    paths: Vec::new(),
                    pretty: None,
                    no_patch: false,
                    context: None,
                    format: None,
                    abbrev: None,
                    stat: false,
                    name_only: false,
                    name_status: false,
                    date: None,
                    color: None,
                    no_color: false,
                },
            );
        }
        if args.skip {
            let head = repository.head()?.oid();
            if let Some(head) = head {
                let tree = repository.commit_tree(head)?;
                repository.reset_hard_to_tree(tree)?;
            }
            clear_pick_state(&repository);
            if interactive::is_interactive(&dir) {
                let _ = std::fs::remove_file(dir.join("stopped-sha"));
                if !interactive::finish_chain_after_skip(global, &mut repository, &dir)? {
                    return interactive::give_up_on_message(&mut repository, &dir);
                }
            }
            return run_todo(global, &mut repository, &dir);
        }
        return continue_rebase(global, &mut repository, &dir);
    }
    start(global, &mut repository, args, &dir)
}

/// Whether the work tree has unstaged tracked changes, and whether the index differs from `HEAD`
/// (or has conflicts).
pub(super) fn dirty_parts(repository: &mut Repository) -> Result<(bool, bool)> {
    let mut options = rivet_core::StatusOptions::new();
    options.untracked_files = rivet_core::UntrackedMode::No;
    let status = repository.status(&options)?;
    let unstaged = status
        .unstaged
        .iter()
        .any(|e| e.kind != rivet_core::ChangeKind::Unmerged);
    let staged = !status.staged.is_empty() || repository.index().has_conflicts();
    Ok((unstaged, staged))
}

/// Whether `--autostash` is in effect: the flag, else `rebase.autoStash`.
pub(super) fn autostash_wanted(repository: &Repository, on: bool, off: bool) -> bool {
    if off {
        return false;
    }
    on || repository
        .config_value(BStr::new(b"rebase.autostash"))
        .ok()
        .flatten()
        .and_then(|v| rivet_core::parse_bool(BStr::new(&v[..])).ok())
        .unwrap_or(false)
}

/// Whether `--autosquash` is in effect for an interactive rebase: the flag, else `rebase.autoSquash`.
fn autosquash_wanted(repository: &Repository, on: bool, off: bool) -> bool {
    if off {
        return false;
    }
    on || repository
        .config_value(BStr::new(b"rebase.autosquash"))
        .ok()
        .flatten()
        .and_then(|v| rivet_core::parse_bool(BStr::new(&v[..])).ok())
        .unwrap_or(false)
}

/// Stash the tracked local changes the way `git rebase --autostash` does: create the stash
/// commit, reset to `HEAD` and say so. `None` when there was nothing to stash.
pub(super) fn create_autostash(repository: &mut Repository) -> Result<Option<ObjectId>> {
    let (unstaged, staged) = dirty_parts(repository)?;
    if !unstaged && !staged {
        return Ok(None);
    }
    let stash = super::stash::autostash_create(repository, "autostash")?;
    println!("Created autostash: {}", super::short_oid(&stash));
    Ok(Some(stash))
}

/// Put an autostash back: silently on success (`Applied autostash.`), otherwise keep it as a
/// stash entry and say how to recover.
pub(super) fn apply_autostash(repository: &mut Repository, stash: ObjectId) -> Result<()> {
    let base = repository
        .commit_parents(stash)?
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("autostash {stash} has no parent"))?;
    if super::stash::autostash_merge(
        repository,
        stash,
        base,
        "Updated upstream",
        "Stashed changes",
    )? {
        super::stash::autostash_store(repository, stash, "autostash")?;
        eprintln!(
            "Your local changes are stashed, however applying them\nresulted in conflicts.  You can either resolve the conflicts\nand then discard the stash with \"git stash drop\", or, if you\ndo not want to resolve them now, run \"git reset --hard\" and\napply the local changes later by running \"git stash pop\"."
        );
    } else {
        eprintln!("Applied autostash.");
    }
    Ok(())
}

/// The autostash a stopped rebase saved, if any.
fn saved_autostash(repository: &Repository, dir: &Path) -> Option<ObjectId> {
    let text = read_state(dir, "autostash");
    (!text.is_empty())
        .then(|| ObjectId::from_hex(repository.format(), BStr::new(text.as_bytes())).ok())
        .flatten()
}

/// Whether the work tree and index are clean enough to rebase; says why not on stderr, as
/// `<what>` (`rebase`, or `pull with rebase`).
pub(super) fn clean_tree(repository: &mut Repository, what: &str) -> Result<bool> {
    let (unstaged, staged) = dirty_parts(repository)?;
    if unstaged {
        eprintln!("error: cannot {what}: You have unstaged changes.");
    }
    if staged {
        if unstaged {
            eprintln!("error: additionally, your index contains uncommitted changes.");
        } else {
            eprintln!("error: cannot {what}: Your index contains uncommitted changes.");
        }
    }
    if unstaged || staged {
        eprintln!("error: Please commit or stash them.");
        return Ok(false);
    }
    Ok(true)
}

fn start(
    global: &GlobalArgs,
    repository: &mut Repository,
    args: &RebaseArgs,
    dir: &Path,
) -> Result<i32> {
    if dir.exists() {
        eprintln!(
            "fatal: It seems that there is already a rebase-merge directory, and\nI wonder if you are in the middle of another rebase.  If that is the\ncase, please try\n\tgit rebase (--continue | --abort | --skip)\nIf that is not the case, please\n\trm -fr \".git/rebase-merge\"\nand run me again.  I am stopping in case you still have something\nvaluable there.\n"
        );
        return Ok(128);
    }
    // The branch to rebase: the one named, or the current one. Naming another branch does not
    // switch to it first; the detach onto the new base moves `HEAD` there in one step.
    // With `--root` there is no upstream, so the one positional argument names the branch.
    let branch_arg = match (args.root, &args.upstream) {
        (true, Some(_)) if args.branch.is_some() => {
            // git has already written the empty commit by the time it checks the positionals.
            if args.onto.is_none() {
                write_virtual_onto(repository)?;
            }
            eprint!("{REBASE_USAGE}");
            return Ok(129);
        }
        (true, upstream) => args.branch.clone().or_else(|| upstream.clone()),
        (false, _) => args.branch.clone(),
    };
    let head_state = repository.head()?;
    let current_branch = match &head_state {
        HeadState::Attached { branch, .. } => Some(branch.short_name().to_string()),
        _ => None,
    };
    let (orig_head, head_name, switch_to) = match &branch_arg {
        Some(branch) => {
            let name = RefName::from_bytes(format!("refs/heads/{branch}").as_bytes())?;
            if repository.refs().exists(&name) {
                let oid = repository.refs().resolve_oid(&name)?;
                let switch = (current_branch.as_deref() != Some(branch.as_str())).then_some(name);
                (oid, format!("refs/heads/{branch}"), switch)
            } else {
                let oid = super::resolve_commit(repository, branch)
                    .map_err(|_| anyhow::anyhow!("invalid branch '{branch}'"))?;
                (oid, "detached HEAD".to_string(), None)
            }
        }
        None => {
            let Some(oid) = head_state.oid() else {
                eprintln!("fatal: Cannot rebase onto an unborn branch");
                return Ok(128);
            };
            let name = match &head_state {
                HeadState::Attached { branch, .. } => {
                    format!("refs/heads/{}", branch.short_name())
                }
                _ => "detached HEAD".to_string(),
            };
            (oid, name, None)
        }
    };
    let label = head_name.clone();

    let upstream_name = match &args.upstream {
        _ if args.root => "--root".to_string(),
        Some(name) => name.clone(),
        None => match tracked_upstream(repository, &head_state)? {
            Some(name) => name,
            None => {
                let branch = match &head_state {
                    HeadState::Attached { branch, .. } => branch.short_name().to_string(),
                    _ => "HEAD".to_string(),
                };
                println!(
                    "There is no tracking information for the current branch.\nPlease specify which branch you want to rebase against.\nSee git-rebase(1) for details.\n\n    git rebase '<branch>'\n\nIf you wish to set tracking information for this branch you can do so with:\n\n    git branch --set-upstream-to=<remote>/<branch> {branch}\n"
                );
                return Ok(1);
            }
        },
    };
    let mut upstream = if args.root {
        None
    } else {
        Some(
            super::resolve_commit(repository, &upstream_name)
                .map_err(|_| anyhow::anyhow!("invalid upstream '{upstream_name}'"))?,
        )
    };
    let mut onto_name = args.onto.clone().unwrap_or_else(|| upstream_name.clone());
    let mut onto = match &args.onto {
        Some(name) => Some(
            super::resolve_commit(repository, name)
                .map_err(|_| anyhow::anyhow!("Does not point to a valid commit '{name}'"))?,
        ),
        None => upstream,
    };
    let mut squash_onto = None;
    if args.root && onto.is_none() {
        let virtual_onto = write_virtual_onto(repository)?;
        onto_name = virtual_onto.to_string();
        onto = Some(virtual_onto);
        squash_onto = onto;
    }
    // With `--root` every commit not reachable from the new base is replayed.
    if args.root {
        upstream = onto;
    }
    let (Some(mut upstream), Some(mut onto)) = (upstream, onto) else {
        unreachable!("an upstream or a base is always known here");
    };
    if args.keep_base {
        // `--keep-base` rebases onto the merge base of upstream and the branch, and that commit
        // also bounds the range, so no patch is skipped as already applied upstream.
        let bases =
            rivet_core::merge_bases(repository.odb(), upstream, orig_head).unwrap_or_default();
        let [base] = bases[..] else {
            eprintln!("fatal: '{upstream_name}': need exactly one merge base with branch");
            return Ok(128);
        };
        let branch = match (&args.branch, &head_state) {
            (Some(branch), _) => branch.clone(),
            (None, HeadState::Attached { branch, .. }) => branch.short_name().to_string(),
            (None, _) => "HEAD".to_string(),
        };
        onto_name = format!("{upstream_name}...{branch}");
        upstream = base;
        onto = base;
    }

    let autostash = if autostash_wanted(repository, args.autostash, args.no_autostash) {
        create_autostash(repository)?
    } else {
        None
    };
    if autostash.is_none() && !clean_tree(repository, "rebase")? {
        return Ok(1);
    }

    // Already based on the target with a linear history: nothing to do.
    let odb = repository.odb();
    let bases = rivet_core::merge_bases(odb, onto, orig_head).unwrap_or_default();
    if !args.interactive && !args.root && onto == upstream && bases.first() == Some(&onto) {
        let mut options = WalkOptions::new();
        options.exclude(onto);
        let between = rivet_core::revwalk::walk_oids(odb, &[orig_head], &options)?;
        let mut linear = true;
        for oid in &between {
            if repository.commit_parents(*oid)?.len() > 1 {
                linear = false;
                break;
            }
        }
        if linear {
            // Naming the branch makes Git settle on it: one reflog line when `HEAD` moves
            // there, two when it is on the branch already.
            if branch_arg.is_some()
                && let Some(branch) = head_name.strip_prefix("refs/heads/")
            {
                let name = RefName::from_bytes(format!("refs/heads/{branch}").as_bytes())?;
                let reason = format!("rebase: checkout {branch}");
                if switch_to.is_some() || !matches!(head_state, HeadState::Attached { .. }) {
                    let tree = repository.commit_tree(orig_head)?;
                    repository.reset_hard_to_tree(tree)?;
                    repository
                        .point_head(HeadTarget::Branch(&name), BStr::new(reason.as_bytes()))?;
                } else {
                    for _ in 0..2 {
                        let who = repository.committer_signature()?;
                        let entry = ReflogEntry::new(
                            orig_head,
                            orig_head,
                            who,
                            BString::from(reason.as_str()),
                        );
                        repository.refs().append_reflog(&RefName::head(), &entry)?;
                    }
                }
            }
            if !global.quiet {
                match head_name.strip_prefix("refs/heads/") {
                    Some(branch) => println!("Current branch {branch} is up to date."),
                    None => println!("HEAD is up to date."),
                }
            }
            if let Some(stash) = autostash {
                apply_autostash(repository, stash)?;
            }
            return Ok(0);
        }
    }

    if !args.no_verify {
        // `pull --rebase` hands the hook the fetched tip, a plain rebase the upstream as typed.
        let mut hook_args = vec![if action().starts_with("pull") {
            onto_name.clone()
        } else {
            upstream_name.clone()
        }];
        hook_args.extend(branch_arg.clone());
        if rebase_hook(repository, "pre-rebase", &hook_args, None, hook_action())
            .is_some_and(|code| code != 0)
        {
            if let Some(stash) = autostash {
                apply_autostash(repository, stash)?;
            }
            eprintln!("error: The pre-rebase hook refused to rebase.");
            return Ok(1);
        }
    }

    // The todo list: non-merge commits of upstream..HEAD, oldest first, minus those whose patch
    // is already on the upstream side.
    let mut options = WalkOptions::new();
    options.exclude(upstream);
    options.sort(SortOrder::Topo);
    let mut range = rivet_core::revwalk::walk_oids(odb, &[orig_head], &options)?;
    range.reverse();
    let mut theirs_options = WalkOptions::new();
    theirs_options.exclude(orig_head);
    let upstream_only = if args.root {
        Vec::new()
    } else {
        rivet_core::revwalk::walk_oids(odb, &[upstream], &theirs_options)?
    };
    let mut upstream_ids: HashSet<Vec<u8>> = HashSet::new();
    for oid in &upstream_only {
        if repository.commit_parents(*oid)?.len() <= 1 {
            upstream_ids.insert(patch_id(repository, oid)?);
        }
    }
    let mut todo: Vec<(ObjectId, String)> = Vec::new();
    let mut skipped = false;
    for oid in range {
        if repository.commit_parents(oid)?.len() > 1 {
            continue;
        }
        if !upstream_ids.is_empty() && upstream_ids.contains(&patch_id(repository, &oid)?) {
            if !global.quiet {
                eprintln!(
                    "warning: skipped previously applied commit {}",
                    super::history::abbrev(repository, &oid, false)
                );
            }
            skipped = true;
            continue;
        }
        todo.push((oid, subject_of(repository, &oid)?));
    }

    if skipped {
        eprintln!("hint: use --reapply-cherry-picks to include skipped commits");
        eprintln!(
            "hint: Disable this message with \"git config set advice.skippedCherryPicks false\""
        );
    }

    if args.interactive {
        let plan = interactive::Plan {
            head_state: &head_state,
            orig_head,
            head_name: &head_name,
            upstream,
            onto,
            onto_name: &onto_name,
            commits: &todo,
            autostash,
            autosquash: autosquash_wanted(repository, args.autosquash, args.no_autosquash),
            exec: &args.exec,
            root: args.root,
            squash_onto,
        };
        return interactive::begin(global, repository, &plan, dir);
    }

    // Leading picks whose parent is already the landing commit change nothing: step over them
    // (only reachable when the branch already sits on the new base, as with `--keep-base`).
    let mut landing = onto;
    let mut skipped = 0;
    for (oid, _) in &todo {
        if repository.commit_parents(*oid)?.as_slice() != [landing] {
            break;
        }
        landing = *oid;
        skipped += 1;
    }

    // Detach at the new base.
    std::fs::create_dir_all(dir)?;
    rivet_core::write_orig_head(repository.git_dir(), orig_head)?;
    let landing_tree = repository.commit_tree(landing)?;
    repository.reset_hard_to_tree(landing_tree)?;
    let reason = format!("{} (start): checkout {onto_name}", action());
    repository.point_head(HeadTarget::Detached(landing), BStr::new(reason.as_bytes()))?;
    // Git runs `post-checkout` once, for the move onto the new base; its status is ignored.
    let zero = ObjectId::zero(repository.format());
    rebase_hook(
        repository,
        "post-checkout",
        &[
            head_state.oid().unwrap_or(zero).to_string(),
            landing.to_string(),
            "1".to_string(),
        ],
        None,
        hook_action(),
    );

    write_state(dir, "head-name", &format!("{head_name}\n"))?;
    write_state(dir, "onto", &format!("{onto}\n"))?;
    write_state(dir, "orig-head", &format!("{orig_head}\n"))?;
    write_state(dir, "end", &format!("{}\n", todo.len()))?;
    write_state(dir, "msgnum", &format!("{skipped}\n"))?;
    write_state(dir, "interactive", "")?;
    let line_of = |(oid, subject): &(ObjectId, String)| format!("pick {oid} # {subject}\n");
    let done: String = todo[..skipped].iter().map(line_of).collect();
    let lines: String = todo[skipped..].iter().map(line_of).collect();
    let all: String = todo.iter().map(line_of).collect();
    write_state(dir, "done", &done)?;
    write_state(dir, "git-rebase-todo", &lines)?;
    write_state(dir, "git-rebase-todo.backup", &all)?;
    if let Some(stash) = autostash {
        write_state(dir, "autostash", &format!("{stash}\n"))?;
    }
    let _ = label;
    run_todo(global, repository, dir)
}

/// The configured upstream of the current branch, as a name `resolve_commit` understands.
pub(super) fn tracked_upstream(
    repository: &Repository,
    head: &HeadState,
) -> Result<Option<String>> {
    let HeadState::Attached { branch, .. } = head else {
        return Ok(None);
    };
    let name = branch.short_name().to_string();
    let remote = repository.config_value(BStr::new(format!("branch.{name}.remote").as_bytes()))?;
    let merge = repository.config_value(BStr::new(format!("branch.{name}.merge").as_bytes()))?;
    let (Some(remote), Some(merge)) = (remote, merge) else {
        return Ok(None);
    };
    let merge = String::from_utf8_lossy(&merge).into_owned();
    let short = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    let remote = String::from_utf8_lossy(&remote).into_owned();
    Ok(Some(if remote == "." {
        format!("refs/heads/{short}")
    } else {
        format!("refs/remotes/{remote}/{short}")
    }))
}

fn clear_pick_state(repository: &Repository) {
    for name in ["REBASE_HEAD", "MERGE_MSG", "AUTO_MERGE"] {
        let _ = std::fs::remove_file(repository.git_dir().join(name));
    }
}

fn abort(repository: &mut Repository, dir: &Path) -> Result<i32> {
    let head_name = read_state(dir, "head-name");
    let orig = read_state(dir, "orig-head");
    let orig = ObjectId::from_hex(repository.format(), BStr::new(orig.as_bytes()))?;
    let tree = repository.commit_tree(orig)?;
    repository.reset_hard_to_tree(tree)?;
    if let Some(branch) = head_name.strip_prefix("refs/heads/") {
        let name = RefName::from_bytes(format!("refs/heads/{branch}").as_bytes())?;
        let reason = format!("{} (abort): returning to {head_name}", action());
        repository.point_head(HeadTarget::Branch(&name), BStr::new(reason.as_bytes()))?;
    } else {
        let reason = format!("{} (abort): returning to {orig}", action());
        repository.point_head(HeadTarget::Detached(orig), BStr::new(reason.as_bytes()))?;
    }
    clear_pick_state(repository);
    let stash = saved_autostash(repository, dir);
    delete_labels(repository, dir);
    std::fs::remove_dir_all(dir)?;
    if let Some(stash) = stash {
        apply_autostash(repository, stash)?;
    }
    Ok(0)
}

fn continue_rebase(global: &GlobalArgs, repository: &mut Repository, dir: &Path) -> Result<i32> {
    if repository.index().has_conflicts() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in repository.index().entries() {
            if entry.is_unmerged() && seen.insert(entry.path.clone()) {
                println!("{}: needs merge", entry.path);
            }
        }
        println!("You must edit all merge conflicts and then\nmark them as resolved using git add");
        return Ok(1);
    }
    if !read_state(dir, "amend").is_empty() {
        // Stopped by `edit`: staged changes amend the commit, anything else just goes on.
        let mut options = rivet_core::StatusOptions::new();
        options.untracked_files = rivet_core::UntrackedMode::No;
        let status = repository.status(&options)?;
        if !status.unstaged.is_empty() {
            println!(
                "You must edit all merge conflicts and then\nmark them as resolved using git add"
            );
            return Ok(1);
        }
        if !status.staged.is_empty() {
            let Some(head) = repository.head()?.oid() else {
                anyhow::bail!("rebase: HEAD is unborn");
            };
            if head.to_string() != read_state(dir, "amend") {
                eprintln!(
                    "error: \nYou have uncommitted changes in your working tree. Please, commit them\nfirst and then run 'git rebase --continue' again."
                );
                return Ok(1);
            }
            let message = read_commit(repository, &head)?.message;
            let tree = repository.write_tree()?;
            interactive::commit_amend(
                repository,
                tree,
                &message,
                "rebase (continue)",
                Some(global.quiet),
            )?;
        }
        clear_pick_state(repository);
        for name in ["amend", "stopped-sha"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        return run_todo(global, repository, dir);
    }
    let stopped = read_state(dir, "stopped-sha");
    if !stopped.is_empty() {
        let oid = ObjectId::from_hex(repository.format(), BStr::new(stopped.as_bytes()))?;
        let original = read_commit(repository, &oid)?;
        let Some(head) = repository.head()?.oid() else {
            anyhow::bail!("rebase: HEAD is unborn");
        };
        let mut options = rivet_core::StatusOptions::new();
        options.untracked_files = rivet_core::UntrackedMode::No;
        if !repository.status(&options)?.unstaged.is_empty() {
            eprintln!("error: cannot rebase: You have unstaged changes.");
            return Ok(1);
        }
        let tree = repository.write_tree()?;
        if repository.head_tree_oid()? == Some(tree) {
            // Resolved to nothing: the commit is dropped and the rebase goes on.
            clear_pick_state(repository);
            let _ = std::fs::remove_file(dir.join("stopped-sha"));
            return run_todo(global, repository, dir);
        }
        let done = read_state(dir, "done");
        let stopped_words: Vec<&str> = done
            .lines()
            .last()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        let stopped_cmd = stopped_words
            .first()
            .and_then(|w| interactive::Cmd::parse(w));
        if let Some(cmd) = stopped_cmd.filter(|c| c.is_fixup()) {
            let (flag, _) = interactive::flag_of(cmd, &stopped_words);
            interactive::chain_add(repository, dir, oid, cmd, flag)?;
            let action = "rebase (continue)";
            if !interactive::chain_commit(global, repository, dir, tree, cmd, action)? {
                return interactive::give_up_on_message(repository, dir);
            }
            interactive::note_pending(dir, oid)?;
            clear_pick_state(repository);
            let _ = std::fs::remove_file(dir.join("stopped-sha"));
            return run_todo(global, repository, dir);
        }
        let saved = std::fs::read(repository.git_dir().join("MERGE_MSG")).unwrap_or_default();
        let mut message = super::branching::strip_comment_lines(&saved);
        if message.iter().all(u8::is_ascii_whitespace) {
            message = original.message.clone();
        }
        let new = commit_pick(
            repository,
            tree,
            head,
            original.author,
            &message,
            "rebase (continue)",
            Some(global.quiet),
        )?;
        record_rewrite(dir, oid, new)?;
        clear_pick_state(repository);
        write_state(dir, "stopped-sha", "")?;
        let _ = std::fs::remove_file(dir.join("stopped-sha"));
    }
    run_todo(global, repository, dir)
}

/// Commit `tree` on top of `parent` with the original author and the current identity as
/// committer, recording `<action>: <subject>` in the reflog. With `Some(quiet)` the commit
/// summary `git commit` prints is shown (the `--continue` case).
fn commit_pick(
    repository: &mut Repository,
    tree: ObjectId,
    parent: ObjectId,
    author: Signature,
    message: &[u8],
    action: &str,
    summary: Option<bool>,
) -> Result<ObjectId> {
    let committer = repository.committer_signature()?;
    let mut builder = rivet_core::CommitBuilder::new();
    builder.tree(tree);
    // A `--root` rebase starts from a virtual empty commit that the first real commit replaces.
    let squash_onto = read_state(&rebase_dir(repository), "squash-onto");
    if squash_onto.trim() != parent.to_string() {
        builder.parent(parent);
    }
    builder.author(author.clone());
    builder.committer(committer.clone());
    builder.message(BStr::new(message));
    let body = builder.build(repository.format())?;
    let oid = repository.odb().write_raw(ObjectKind::Commit, &body)?;
    let subject = message.lines().next().unwrap_or_default();
    let reason = format!("{action}: {}", String::from_utf8_lossy(subject));
    repository.move_head(oid, BStr::new(reason.as_bytes()))?;
    // `--continue` is a `git commit` that exports its own reflog action; a pick exports one only
    // under `pull`.
    let hook_env = if action == "rebase (continue)" {
        Some(action)
    } else {
        hook_action()
    };
    super::snapshot::commit_hook(repository, "post-commit", &[], hook_env);
    if let Some(false) = summary {
        println!(
            "[detached HEAD {}] {}",
            super::short_oid(&oid),
            super::commit_subject(repository, &oid)?
        );
        if author.name != committer.name || author.email != committer.email {
            println!(" Author: {} <{}>", author.name, author.email);
        }
        let old_tree = Some(repository.commit_tree(parent)?);
        super::print_shortstat_summary(repository, old_tree, tree)?;
    }
    Ok(oid)
}

/// `GIT_AUTHOR_*` assignments for `author-script`, the author the stopped commit keeps.
fn author_script(author: &Signature) -> String {
    format!(
        "GIT_AUTHOR_NAME='{}'\nGIT_AUTHOR_EMAIL='{}'\nGIT_AUTHOR_DATE='@{} {}'\n",
        author.name,
        author.email,
        author.when,
        tz_text(author.tz_offset_minutes)
    )
}

/// Stop after an `edit` command has put its commit in place.
fn stop_for_edit(
    repository: &Repository,
    dir: &Path,
    oid: ObjectId,
    original: &Commit,
) -> Result<i32> {
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("rebase: HEAD is unborn");
    };
    write_state(dir, "amend", &format!("{head}\n"))?;
    write_state(dir, "stopped-sha", &format!("{oid}\n"))?;
    write_state(dir, "message", &String::from_utf8_lossy(&original.message))?;
    write_state(dir, "author-script", &author_script(&original.author))?;
    eprintln!(
        "Stopped at {}...  # {}\nYou can amend the commit now, with\n\n  git commit --amend \n\nOnce you are satisfied with your changes, run\n\n  git rebase --continue",
        super::history::abbrev(repository, &oid, false),
        subject_of(repository, &oid)?
    );
    Ok(0)
}

/// Run the remaining todo lines; finish the rebase when none are left.
fn run_todo(global: &GlobalArgs, repository: &mut Repository, dir: &Path) -> Result<i32> {
    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(repository, "merge.renames");
    let interactive = interactive::is_interactive(dir);
    loop {
        let todo_text = read_state(dir, "git-rebase-todo");
        let mut lines: Vec<&str> = todo_text.lines().collect();
        if lines.is_empty() {
            break;
        }
        let line = lines.remove(0).to_string();
        let word = line.split_whitespace().next().unwrap_or_default();
        let Some(cmd) = interactive::Cmd::parse(word) else {
            eprintln!("error: invalid command '{word}'");
            eprintln!("error: invalid line 1: {line}");
            eprintln!("error: please fix this using 'git rebase --edit-todo'.");
            return Ok(1);
        };
        let rest: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut done = read_state(dir, "done");
        if !done.is_empty() {
            done.push('\n');
        }
        done.push_str(&line);
        done.push('\n');
        let counted = cmd != interactive::Cmd::Noop;
        let msgnum: usize = read_state(dir, "msgnum").parse().unwrap_or(0) + usize::from(counted);
        // Git counts what is done plus what is left each time, so an edited list shows its new size.
        let end = [&done, &rest]
            .iter()
            .flat_map(|text| text.lines())
            .filter(|l| l.split_whitespace().next() != Some("noop") && !l.trim().is_empty())
            .count();
        write_state(dir, "git-rebase-todo", &rest)?;
        write_state(dir, "done", &done)?;
        write_state(dir, "msgnum", &format!("{msgnum}\n"))?;
        if !counted {
            continue;
        }
        if interactive && !cmd.is_fixup() {
            interactive::flush_pending(repository, dir)?;
        }

        if cmd == interactive::Cmd::Break {
            if !global.quiet {
                eprint!("Rebasing ({msgnum}/{end})\r");
            }
            let Some(head) = repository.head()?.oid() else {
                anyhow::bail!("rebase: HEAD is unborn");
            };
            eprintln!(
                "Stopped at {} ({})",
                super::history::abbrev(repository, &head, false),
                subject_of(repository, &head)?
            );
            return Ok(0);
        }

        if cmd == interactive::Cmd::Exec {
            if !global.quiet {
                eprint!("Rebasing ({msgnum}/{end})\r");
            }
            let command = line[line.find(char::is_whitespace).unwrap_or(line.len())..].trim();
            if !run_exec(repository, command) {
                eprintln!(
                    "warning: execution failed: {command}\nYou can fix the problem, and then run\n\n  git rebase --continue\n\n"
                );
                return Ok(1);
            }
            continue;
        }

        if matches!(
            cmd,
            interactive::Cmd::Label | interactive::Cmd::Reset | interactive::Cmd::UpdateRef
        ) {
            if !global.quiet {
                eprint!("Rebasing ({msgnum}/{end})\r");
            }
            let target = line[line.find(char::is_whitespace).unwrap_or(line.len())..].trim();
            let ok = if cmd == interactive::Cmd::Label {
                run_label(repository, dir, target)?
            } else if cmd == interactive::Cmd::UpdateRef {
                run_update_ref(repository, dir, target)?
            } else {
                run_reset(repository, target)?
            };
            if !ok {
                write_state(dir, "git-rebase-todo", &format!("{line}\n{rest}"))?;
                eprintln!(
                    "hint: Could not execute the todo command\nhint:\nhint:     {line}\nhint:\nhint: It has been rescheduled; To edit the command before continuing, please\nhint: edit the todo list first:\nhint:\nhint:     git rebase --edit-todo\nhint:     git rebase --continue"
                );
                return Ok(1);
            }
            continue;
        }

        let words: Vec<&str> = line.split_whitespace().collect();
        let (flag, hex) = interactive::flag_of(cmd, &words);
        let hex = hex.ok_or_else(|| anyhow::anyhow!("rebase: malformed todo line '{line}'"))?;
        let oid = ObjectId::from_hex(repository.format(), BStr::new(hex.as_bytes()))?;
        let original = read_commit(repository, &oid)?;
        let subject = subject_of(repository, &oid)?;
        if !global.quiet {
            eprint!("Rebasing ({msgnum}/{end})\r");
        }
        if cmd == interactive::Cmd::Drop {
            continue;
        }
        let Some(head) = repository.head()?.oid() else {
            anyhow::bail!("rebase: HEAD is unborn");
        };

        // A commit that already sits on the current head is reused, not rewritten. A root commit
        // is reused on top of the empty commit that `--root` starts from.
        let on_virtual_root = original.parents.is_empty() && is_virtual_onto(repository, &head)?;
        if !cmd.is_fixup() && (original.parents.first() == Some(&head) || on_virtual_root) {
            let reason = format!("{}: fast-forward", action());
            repository.move_head(oid, BStr::new(reason.as_bytes()))?;
            let tree = repository.commit_tree(oid)?;
            repository.reset_hard_to_tree(tree)?;
            match cmd {
                interactive::Cmd::Reword => {
                    if !interactive::reword_head(global, repository)? {
                        return interactive::give_up_on_message(repository, dir);
                    }
                    interactive::note_pending(dir, oid)?;
                }
                interactive::Cmd::Edit => return stop_for_edit(repository, dir, oid, &original),
                _ => {}
            }
            continue;
        }

        let short = super::history::abbrev(repository, &oid, false);
        let label = format!("{short} ({subject})");
        let mut merge_options = options.clone();
        merge_options.labels = (BString::from("HEAD"), BString::from(label.as_str()));
        let base = original.parents.first().copied();
        let result = match repository.merge_into_worktree(base, oid, &merge_options) {
            Ok(result) => result,
            Err(error) => {
                super::branching::report_checkout_refusal(error, "merge")?;
                write_state(dir, "stopped-sha", &format!("{oid}\n"))?;
                eprintln!("Could not apply {short}... # {subject}");
                return Ok(1);
            }
        };
        let conflicted = super::branching::print_merge_report(&result, &label, &[]);
        if !conflicted.is_empty() {
            let git_dir = repository.git_dir().to_path_buf();
            let mut merge_msg = original.message.clone();
            merge_msg.extend_from_slice(b"\n# Conflicts:\n");
            for path in &conflicted {
                merge_msg.extend_from_slice(b"#\t");
                merge_msg.extend_from_slice(path);
                merge_msg.push(b'\n');
            }
            std::fs::write(git_dir.join("MERGE_MSG"), &merge_msg)?;
            std::fs::write(git_dir.join("REBASE_HEAD"), format!("{oid}\n"))?;
            write_state(dir, "stopped-sha", &format!("{oid}\n"))?;
            write_state(dir, "message", &String::from_utf8_lossy(&merge_msg))?;
            write_state(dir, "author-script", &author_script(&original.author))?;
            eprintln!("error: could not apply {short}... {subject}");
            eprintln!("hint: Resolve all conflicts manually, mark them as resolved with");
            eprintln!(
                "hint: \"git add/rm <conflicted_files>\", then run \"git rebase --continue\"."
            );
            eprintln!("hint: You can instead skip this commit: run \"git rebase --skip\".");
            eprintln!(
                "hint: To abort and get back to the state before \"git rebase\", run \"git rebase --abort\"."
            );
            eprintln!(
                "hint: Disable this message with \"git config set advice.mergeConflict false\""
            );
            if cmd.is_fixup() {
                eprintln!(
                    "You can amend the commit now, with\n\n  git commit --amend \n\nOnce you are satisfied with your changes, run\n\n  git rebase --continue"
                );
            } else {
                eprintln!("Could not apply {short}... # {subject}");
            }
            return Ok(1);
        }
        let tree = repository.write_tree()?;
        if cmd.is_fixup() {
            interactive::chain_add(repository, dir, oid, cmd, flag)?;
            let action = format!("{} ({})", action(), cmd.name());
            if !interactive::chain_commit(global, repository, dir, tree, cmd, &action)? {
                return interactive::give_up_on_message(repository, dir);
            }
            interactive::note_pending(dir, oid)?;
            continue;
        }
        if repository.head_tree_oid()? == Some(tree) {
            if !global.quiet {
                eprintln!("dropping {oid} {subject} -- patch contents already upstream");
            }
            continue;
        }
        let new = commit_pick(
            repository,
            tree,
            head,
            original.author.clone(),
            &original.message,
            &format!("{} ({})", action(), cmd.name()),
            None,
        )?;
        if interactive {
            interactive::note_pending(dir, oid)?;
        } else {
            record_rewrite(dir, oid, new)?;
        }
        match cmd {
            interactive::Cmd::Reword => {
                if !interactive::reword_head(global, repository)? {
                    return interactive::give_up_on_message(repository, dir);
                }
            }
            interactive::Cmd::Edit => return stop_for_edit(repository, dir, oid, &original),
            _ => {}
        }
    }
    finish(global, repository, dir)
}

/// `label <name>`: remember HEAD as `refs/rewritten/<name>`, to be removed when the rebase ends.
fn run_label(repository: &Repository, dir: &Path, name: &str) -> Result<bool> {
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("rebase: HEAD is unborn");
    };
    let path = repository.git_dir().join("refs/rewritten").join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{head}\n"))?;
    let mut list = std::fs::read_to_string(dir.join("refs-to-delete")).unwrap_or_default();
    let entry = format!("refs/rewritten/{name}");
    if !list.lines().any(|l| l == entry) {
        list.push_str(&format!("{entry}\n"));
        std::fs::write(dir.join("refs-to-delete"), list)?;
    }
    Ok(true)
}

/// `reset <label or commit>`: move HEAD there and make the work tree match. `false` (after git's
/// error line) when the name resolves to nothing.
fn run_reset(repository: &mut Repository, target: &str) -> Result<bool> {
    let label = repository.git_dir().join("refs/rewritten").join(target);
    let from_label = std::fs::read_to_string(label).ok().and_then(|text| {
        ObjectId::from_hex(repository.format(), BStr::new(text.trim().as_bytes())).ok()
    });
    let resolved = match from_label {
        Some(oid) => Some(oid),
        None => crate::cmd::resolve_commit(repository, target).ok(),
    };
    let Some(oid) = resolved else {
        eprintln!("error: could not resolve '{target}'");
        return Ok(false);
    };
    if repository.head()?.oid() != Some(oid) {
        let reason = format!("{} (reset): '{target}'", action());
        repository.move_head(oid, BStr::new(reason.as_bytes()))?;
        let tree = repository.commit_tree(oid)?;
        repository.reset_hard_to_tree(tree)?;
    }
    Ok(true)
}

/// Write `update-refs`, git's list of the refs the `update-ref` lines name: for each, one line
/// with the ref, one with its id now (all zeros when it does not exist) and one for the id it will
/// get (all zeros until the line runs).
fn write_update_refs(
    repository: &Repository,
    dir: &Path,
    items: &[interactive::Item],
) -> Result<()> {
    let zero = ObjectId::zero(repository.format());
    let mut text = String::new();
    let mut seen: Vec<&str> = Vec::new();
    for item in items
        .iter()
        .filter(|i| i.cmd == interactive::Cmd::UpdateRef)
    {
        let name = item.text.as_deref().unwrap_or_default();
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        let before = RefName::from_bytes(name.as_bytes())
            .ok()
            .and_then(|r| repository.refs().resolve_oid(&r).ok())
            .unwrap_or(zero);
        text.push_str(&format!("{name}\n{before}\n{zero}\n"));
    }
    if !text.is_empty() {
        write_state(dir, "update-refs", &text)?;
    }
    Ok(())
}

/// `update-ref <ref>`: remember HEAD as the id `<ref>` gets when the rebase finishes.
fn run_update_ref(repository: &Repository, dir: &Path, name: &str) -> Result<bool> {
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("rebase: HEAD is unborn");
    };
    let text = read_state(dir, "update-refs");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match lines.iter().position(|l| l == name).filter(|i| i % 3 == 0) {
        Some(i) => lines[i + 2] = head.to_string(),
        None => {
            let zero = ObjectId::zero(repository.format());
            lines.extend([name.to_string(), zero.to_string(), head.to_string()]);
        }
    }
    write_state(dir, "update-refs", &format!("{}\n", lines.join("\n")))?;
    Ok(true)
}

/// Move the refs the `update-ref` lines collected; returns the ones that moved.
fn apply_update_refs(repository: &mut Repository, dir: &Path) -> Result<Vec<String>> {
    let text = read_state(dir, "update-refs");
    let lines: Vec<&str> = text.lines().collect();
    let zero = ObjectId::zero(repository.format()).to_string();
    let who = repository.committer_signature()?;
    let mut moved = Vec::new();
    for entry in lines.as_chunks::<3>().0 {
        if entry[2] == zero {
            continue;
        }
        let name = RefName::from_bytes(entry[0].as_bytes())?;
        let oid = ObjectId::from_hex(repository.format(), BStr::new(entry[2].as_bytes()))?;
        repository.set_ref_logged(&name, oid, &who, BStr::new(b"rewritten during rebase"))?;
        moved.push(entry[0].to_string());
    }
    Ok(moved)
}

/// Remove the `refs/rewritten/*` refs the `label` lines of this rebase made.
fn delete_labels(repository: &Repository, dir: &Path) {
    for entry in read_state(dir, "refs-to-delete").lines() {
        let _ = std::fs::remove_file(repository.git_dir().join(entry));
    }
}

/// Write the empty commit `rebase --root` replays onto when there is no `--onto`; git writes it
/// without a message and with the empty tree.
fn write_virtual_onto(repository: &Repository) -> Result<ObjectId> {
    let tree = super::empty_tree(repository)?;
    let mut builder = rivet_core::CommitBuilder::new();
    builder.tree(tree);
    builder.author(repository.author_signature()?);
    builder.committer(repository.committer_signature()?);
    builder.message(BStr::new(b""));
    let body = builder.build(repository.format())?;
    Ok(repository.odb().write_raw(ObjectKind::Commit, &body)?)
}

/// Whether `oid` is the empty commit `rebase --root` starts from: no parent, the empty tree and
/// no message.
fn is_virtual_onto(repository: &Repository, oid: &ObjectId) -> Result<bool> {
    let commit = read_commit(repository, oid)?;
    Ok(commit.parents.is_empty()
        && commit.message.is_empty()
        && repository.commit_tree(*oid)? == super::empty_tree(repository)?)
}

/// Run an `exec` line in the work tree the way git does: a bare word is started directly, anything
/// else goes through `sh -c`. Its stdout is ours; `Executing:` goes to stderr. `false` when it failed.
fn run_exec(repository: &Repository, command: &str) -> bool {
    eprintln!("Executing: {command}");
    let bare = !command.contains(|c: char| "|&;<>()$`\\\"' \t\n*?[#~=%".contains(c));
    let mut process = if bare {
        std::process::Command::new(command)
    } else {
        let mut sh = std::process::Command::new("sh");
        sh.arg("-c").arg(command);
        sh
    };
    if let Some(root) = repository.worktree_path() {
        process.current_dir(root);
    }
    match process.status() {
        Ok(status) => status.success(),
        Err(error) => {
            let reason = match error.kind() {
                std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
                _ => error.to_string(),
            };
            eprintln!("error: cannot run {command}: {reason}");
            false
        }
    }
}

/// Append `<old> <new>` to the rewritten list that `post-rewrite` reads at the end.
fn record_rewrite(dir: &Path, old: ObjectId, new: ObjectId) -> Result<()> {
    use std::io::Write as _;
    let mut list = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("rewritten-list"))?;
    writeln!(list, "{old} {new}")?;
    Ok(())
}

fn tz_text(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    format!("{sign}{:02}{:02}", minutes.abs() / 60, minutes.abs() % 60)
}

fn finish(global: &GlobalArgs, repository: &mut Repository, dir: &Path) -> Result<i32> {
    let head_name = read_state(dir, "head-name");
    let onto = read_state(dir, "onto");
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("rebase: HEAD is unborn");
    };
    if let Some(branch) = head_name.strip_prefix("refs/heads/") {
        let name = RefName::from_bytes(format!("refs/heads/{branch}").as_bytes())?;
        let who = repository.committer_signature()?;
        let reason = format!("{} (finish): {head_name} onto {onto}", action());
        // A branch that already points at the result gets no reflog line.
        if repository.refs().resolve_oid(&name).ok() != Some(head) {
            repository.set_ref_logged(&name, head, &who, BStr::new(reason.as_bytes()))?;
        }
        let reason = format!("{} (finish): returning to {head_name}", action());
        repository.point_head(HeadTarget::Branch(&name), BStr::new(reason.as_bytes()))?;
    }
    clear_pick_state(repository);
    interactive::flush_pending(repository, dir)?;
    let rewritten = std::fs::read(dir.join("rewritten-list")).unwrap_or_default();
    if !rewritten.is_empty() {
        rebase_hook(
            repository,
            "post-rewrite",
            &["rebase".to_string()],
            Some(rewritten),
            hook_action(),
        );
    }
    let updated = apply_update_refs(repository, dir)?;
    let stash = saved_autostash(repository, dir);
    delete_labels(repository, dir);
    std::fs::remove_dir_all(dir)?;
    if let Some(stash) = stash {
        apply_autostash(repository, stash)?;
    }
    if !global.quiet {
        match head_name.strip_prefix("refs/heads/") {
            Some(_) => eprintln!("Successfully rebased and updated {head_name}."),
            None => eprintln!("Successfully rebased and updated detached HEAD."),
        }
        if !updated.is_empty() {
            eprintln!("Updated the following refs with --update-refs:");
            for name in updated {
                eprintln!("\t{name}");
            }
        }
    }
    Ok(0)
}
