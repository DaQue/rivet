//! `rebase`: replay the commits of the current branch on top of another base (the linear case).
//!
//! # What is implemented
//!
//! * `rebase <upstream> [<branch>]` and `--onto <newbase> <upstream>`: the commits of
//!   `<upstream>..<branch>` are replayed one at a time, merge commits are flattened away, and a
//!   commit whose patch already exists upstream is dropped, exactly as Git's merge backend does.
//! * `--continue`, `--skip` and `--abort` over a stopped rebase.
//! * The state lives in `.git/rebase-merge/` with Git's file names (`head-name`, `onto`,
//!   `orig-head`, `done`, `git-rebase-todo`, `msgnum`, `end`, `stopped-sha`, `message`,
//!   `author-script`), so `status` can describe it.
//!
//! # What is refused
//!
//! `--interactive`, `--root`, `--rebase-merges`, `--autostash`, `--exec`, `--keep-base`, signing
//! and the other options Git has; the grammar does not accept them, so they fail at the command
//! line instead of being ignored.
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

/// Arguments of `rebase`.
#[derive(Debug, clap::Args)]
pub struct RebaseArgs {
    /// The upstream to rebase onto (default: the branch's configured upstream).
    #[arg(value_name = "UPSTREAM")]
    pub upstream: Option<String>,

    /// The branch to rebase (default: the current one); it is switched to first.
    #[arg(value_name = "BRANCH")]
    pub branch: Option<String>,

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
fn patch_id(repository: &Repository, oid: &ObjectId) -> Result<Vec<u8>> {
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

fn subject_of(repository: &Repository, oid: &ObjectId) -> Result<String> {
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
    let mut repository = super::open(global)?;
    let dir = rebase_dir(&repository);
    let controls = [args.continue_rebase, args.abort, args.skip]
        .iter()
        .filter(|on| **on)
        .count();
    if controls > 1 {
        anyhow::bail!("options '--continue', '--abort' and '--skip' cannot be used together");
    }
    if controls == 1 {
        if !dir.is_dir() {
            eprintln!("fatal: no rebase in progress");
            return Ok(128);
        }
        if args.abort {
            return abort(&mut repository, &dir);
        }
        if args.skip {
            let head = repository.head()?.oid();
            if let Some(head) = head {
                let tree = repository.commit_tree(head)?;
                repository.reset_hard_to_tree(tree)?;
            }
            clear_pick_state(&repository);
            return run_todo(global, &mut repository, &dir);
        }
        return continue_rebase(global, &mut repository, &dir);
    }
    start(global, &mut repository, args, &dir)
}

fn clean_tree(repository: &mut Repository) -> Result<bool> {
    let mut options = rivet_core::StatusOptions::new();
    options.untracked_files = rivet_core::UntrackedMode::No;
    let status = repository.status(&options)?;
    let unstaged = status
        .unstaged
        .iter()
        .any(|e| e.kind != rivet_core::ChangeKind::Unmerged);
    let staged = !status.staged.is_empty() || repository.index().has_conflicts();
    if unstaged {
        eprintln!("error: cannot rebase: You have unstaged changes.");
    }
    if staged {
        if unstaged {
            eprintln!("error: additionally, your index contains uncommitted changes.");
        } else {
            eprintln!("error: cannot rebase: Your index contains uncommitted changes.");
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
    let head_state = repository.head()?;
    let current_branch = match &head_state {
        HeadState::Attached { branch, .. } => Some(branch.short_name().to_string()),
        _ => None,
    };
    let (orig_head, head_name, switch_to) = match &args.branch {
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
    let upstream = super::resolve_commit(repository, &upstream_name)
        .map_err(|_| anyhow::anyhow!("invalid upstream '{upstream_name}'"))?;
    let onto_name = args.onto.clone().unwrap_or_else(|| upstream_name.clone());
    let onto = match &args.onto {
        Some(name) => super::resolve_commit(repository, name)
            .map_err(|_| anyhow::anyhow!("Does not point to a valid commit '{name}'"))?,
        None => upstream,
    };

    if !clean_tree(repository)? {
        return Ok(1);
    }

    // Already based on the target with a linear history: nothing to do.
    let odb = repository.odb();
    let bases = rivet_core::merge_bases(odb, onto, orig_head).unwrap_or_default();
    if onto == upstream && bases.first() == Some(&onto) {
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
            if args.branch.is_some()
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
            return Ok(0);
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
    let upstream_only = rivet_core::revwalk::walk_oids(odb, &[upstream], &theirs_options)?;
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
            eprintln!(
                "warning: skipped previously applied commit {}",
                super::history::abbrev(repository, &oid, false)
            );
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

    // Detach at the new base.
    std::fs::create_dir_all(dir)?;
    rivet_core::write_orig_head(repository.git_dir(), orig_head)?;
    let onto_tree = repository.commit_tree(onto)?;
    repository.reset_hard_to_tree(onto_tree)?;
    let reason = format!("rebase (start): checkout {onto_name}");
    repository.point_head(HeadTarget::Detached(onto), BStr::new(reason.as_bytes()))?;

    write_state(dir, "head-name", &format!("{head_name}\n"))?;
    write_state(dir, "onto", &format!("{onto}\n"))?;
    write_state(dir, "orig-head", &format!("{orig_head}\n"))?;
    write_state(dir, "end", &format!("{}\n", todo.len()))?;
    write_state(dir, "msgnum", "0\n")?;
    write_state(dir, "interactive", "")?;
    write_state(dir, "done", "")?;
    let lines: String = todo
        .iter()
        .map(|(oid, subject)| format!("pick {oid} # {subject}\n"))
        .collect();
    write_state(dir, "git-rebase-todo", &lines)?;
    write_state(dir, "git-rebase-todo.backup", &lines)?;
    let _ = label;
    run_todo(global, repository, dir)
}

/// The configured upstream of the current branch, as a name `resolve_commit` understands.
fn tracked_upstream(repository: &Repository, head: &HeadState) -> Result<Option<String>> {
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
        let reason = format!("rebase (abort): returning to {head_name}");
        repository.point_head(HeadTarget::Branch(&name), BStr::new(reason.as_bytes()))?;
    } else {
        let reason = format!("rebase (abort): returning to {orig}");
        repository.point_head(HeadTarget::Detached(orig), BStr::new(reason.as_bytes()))?;
    }
    clear_pick_state(repository);
    std::fs::remove_dir_all(dir)?;
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
        let saved = std::fs::read(repository.git_dir().join("MERGE_MSG")).unwrap_or_default();
        let mut message = super::branching::strip_comment_lines(&saved);
        if message.iter().all(u8::is_ascii_whitespace) {
            message = original.message.clone();
        }
        commit_pick(
            repository,
            tree,
            head,
            original.author,
            &message,
            "rebase (continue)",
            Some(global.quiet),
        )?;
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
    builder.parent(parent);
    builder.author(author.clone());
    builder.committer(committer.clone());
    builder.message(BStr::new(message));
    let body = builder.build(repository.format())?;
    let oid = repository.odb().write_raw(ObjectKind::Commit, &body)?;
    let subject = message.lines().next().unwrap_or_default();
    let reason = format!("{action}: {}", String::from_utf8_lossy(subject));
    repository.move_head(oid, BStr::new(reason.as_bytes()))?;
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

/// Run the remaining todo lines; finish the rebase when none are left.
fn run_todo(global: &GlobalArgs, repository: &mut Repository, dir: &Path) -> Result<i32> {
    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(repository, "merge.renames");
    loop {
        let todo_text = read_state(dir, "git-rebase-todo");
        let mut lines: Vec<&str> = todo_text.lines().collect();
        if lines.is_empty() {
            break;
        }
        let line = lines.remove(0).to_string();
        let rest: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut done = read_state(dir, "done");
        if !done.is_empty() {
            done.push('\n');
        }
        done.push_str(&line);
        done.push('\n');
        let msgnum: usize = read_state(dir, "msgnum").parse().unwrap_or(0) + 1;
        let end = read_state(dir, "end");
        write_state(dir, "git-rebase-todo", &rest)?;
        write_state(dir, "done", &done)?;
        write_state(dir, "msgnum", &format!("{msgnum}\n"))?;

        let hex = line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("rebase: malformed todo line '{line}'"))?;
        let oid = ObjectId::from_hex(repository.format(), BStr::new(hex.as_bytes()))?;
        let original = read_commit(repository, &oid)?;
        let subject = subject_of(repository, &oid)?;
        if !global.quiet {
            eprint!("Rebasing ({msgnum}/{end})\r");
        }
        let Some(head) = repository.head()?.oid() else {
            anyhow::bail!("rebase: HEAD is unborn");
        };

        // A commit that already sits on the current head is reused, not rewritten.
        if original.parents.first() == Some(&head) {
            let reason = format!("rebase (pick): {subject}");
            repository.move_head(oid, BStr::new(reason.as_bytes()))?;
            let tree = repository.commit_tree(oid)?;
            repository.reset_hard_to_tree(tree)?;
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
            write_state(
                dir,
                "author-script",
                &format!(
                    "GIT_AUTHOR_NAME='{}'\nGIT_AUTHOR_EMAIL='{}'\nGIT_AUTHOR_DATE='@{} {}'\n",
                    original.author.name,
                    original.author.email,
                    original.author.when,
                    tz_text(original.author.tz_offset_minutes)
                ),
            )?;
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
            eprintln!("Could not apply {short}... # {subject}");
            return Ok(1);
        }
        let tree = repository.write_tree()?;
        if repository.head_tree_oid()? == Some(tree) {
            if !global.quiet {
                eprintln!("dropping {oid} {subject} -- patch contents already upstream");
            }
            continue;
        }
        commit_pick(
            repository,
            tree,
            head,
            original.author,
            &original.message,
            "rebase (pick)",
            None,
        )?;
    }
    finish(global, repository, dir)
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
        let reason = format!("rebase (finish): {head_name} onto {onto}");
        repository.set_ref_logged(&name, head, &who, BStr::new(reason.as_bytes()))?;
        let reason = format!("rebase (finish): returning to {head_name}");
        repository.point_head(HeadTarget::Branch(&name), BStr::new(reason.as_bytes()))?;
        if !global.quiet {
            eprintln!("Successfully rebased and updated {head_name}.");
        }
    } else {
        if !global.quiet {
            eprintln!("Successfully rebased and updated detached HEAD.");
        }
    }
    clear_pick_state(repository);
    std::fs::remove_dir_all(dir)?;
    Ok(0)
}
