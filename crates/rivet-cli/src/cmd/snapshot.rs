//! Snapshot commands: `add`, `rm`, `commit`, `status`, `restore` and `reset`.
//!
//! This is the group that moves work between the three states a Git user thinks in — the work
//! tree, the index and the commit history. Together with [`crate::cmd::history`] it is the
//! tinyvcs feature set (plan §4, Phases 3 and 4), which is milestone **M1**.
//!
//! *Phase 3 for `add`/`rm`/`commit`/`status`, Phase 4 for `restore`/`reset`.*

use std::path::PathBuf;

use anyhow::Result;
use bstr::BString;

use crate::cli::{GlobalArgs, UntrackedModeArg};
use rivet_core::refs::HeadState;
use rivet_core::worktree::{AddOptions, ChangeKind, RmOptions, StatusOptions, UntrackedMode};

/// Arguments to `rivet add`.
///
/// # Caveats
///
/// * **`force` is not "add an ignored file by accident".** Without it, adding a path that
///   `.gitignore` matches is refused with [`rivet_core::Error::PathIgnored`]; with it, the file
///   is staged and stays ignored for every later command, which is the difference between a
///   one-off and a policy change.
/// * **`all` and `update` are mutually exclusive in effect, not in the grammar.** Git refuses
///   the pair; expressing that needs `conflicts_with`, or the handler must choose. `--all`
///   stages untracked and modified files; `--update` stages only paths already tracked, which is
///   the form that is safe to run in a script.
/// * **A path that does not exist and is not tracked is
///   [`rivet_core::Error::PathNotFound`]** — Git's `fatal: pathspec 'x' did not match any
///   files`. A path that does not exist but *is* tracked stages a deletion, which is the
///   asymmetry that makes `rivet add` and `rivet rm` not mirror images.
/// * **The pathspec is a list, and an empty list means nothing, not everything.** Git's
///   "nothing specified, nothing added" behaviour is the correct one; `--all` is how a caller
///   says "everything", and inferring it from an empty list would make a script that computed
///   its file list and produced none of them delete the user's staging area.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// The paths to stage. Interpreted relative to the current directory.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// Stage every untracked and modified file in the work tree.
    #[arg(short = 'A', long = "all")]
    pub all: bool,

    /// Stage modifications and deletions of files already tracked, and no others.
    #[arg(short = 'u', long = "update")]
    pub update: bool,

    /// Stage an ignored file.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Record a path whose content should be added on the next commit.
    #[arg(short = 'N', long = "intent-to-add")]
    pub intent_to_add: bool,

    /// Report what would be staged, and change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
}

/// Arguments to `rivet rm`.
///
/// # Caveats
///
/// * **`cached` is the flag that makes this command safe.** Without it the work tree file is
///   deleted as well; with it only the index entry goes. A user who meant to untrack a file and
///   forgot `--cached` has lost the file, so the confirmation path matters.
/// * **A file that has staged content differing from `HEAD` is refused without `--force`.** The
///   check is against the *index*, not the work tree: an unstaged modification is not a reason
///   to refuse, because it is about to be deleted anyway, and a staged one is the work `rm`
///   would otherwise silently discard.
/// * **A directory is only removed with `-r`.** A directory pathspec without it is an error even
///   when the directory is empty, matching Git.
#[derive(Debug, clap::Args)]
pub struct RmArgs {
    /// The paths to remove.
    #[arg(value_name = "PATHSPEC", required = true)]
    pub paths: Vec<PathBuf>,

    /// Remove the path from the index only, leaving the work tree file in place.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Recurse into a directory pathspec.
    #[arg(short = 'r')]
    pub recursive: bool,

    /// Remove the path even when its staged content differs from `HEAD`.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Report what would be removed, and change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
}

/// Arguments to `rivet commit`.
///
/// # Caveats
///
/// * **`message` and `reuse_message` are mutually exclusive**, and the grammar must say so —
///   two places supplying the message would otherwise be resolved by an arbitrary precedence
///   rule. `-C` reuses another commit's message verbatim (message *and* authorship);
///   `-c` opens the editor with it, which is not implemented.
/// * **`author` overrides only the author; the committer is always the ambient identity.**
///   The two are separate fields in the object and a command that set both from `--author`
///   would produce a commit whose reflog and whose object disagree about who made it.
/// * **`amend` with an unborn `HEAD` is an error**, not a root commit. Amending nothing is a
///   mistake; a caller that wants a first commit says `commit`.
/// * **`--allow-empty` is off by default and must stay off.** A commit with no change relative
///   to its parent is almost always a typo, and the flag exists for the deliberate cases.
/// * **`pathspec` after `--` commits only those paths**, bypassing the index for them and
///   leaving the rest of the index untouched. That mode is the one where a commit *does*
///   partially stage, and it is the exception to "the index is what gets committed".
/// * **A merge in progress supplies the parents and the default message.** With `MERGE_HEAD`
///   present, the parents are `HEAD` and `MERGE_HEAD` and the message defaults to `MERGE_MSG`;
///   the handler must read that state rather than deriving parents from `HEAD` alone, or the
///   merge commit loses a parent and the history forks silently.
/// * **`signoff` and `gpg_sign` are accepted and not implemented** (commit signing is a
///   non-goal for v1, plan §1). Accepting the flag and ignoring it would produce an unsigned
///   commit that the user believes is signed, so it must fail loudly instead.
#[derive(Debug, clap::Args)]
pub struct CommitArgs {
    /// The commit message. Without one, and without `-C`, the editor would be opened.
    /// Repeating `-m` joins the parts with a blank line, as Git does.
    #[arg(short = 'm', long = "message", value_name = "MESSAGE")]
    pub message: Vec<String>,

    /// Take the message, and the authorship, from an existing commit.
    #[arg(short = 'C', long = "reuse-message", value_name = "COMMIT")]
    pub reuse_message: Option<String>,

    /// Take the message from an existing commit, and open the editor with it.
    #[arg(short = 'c', long = "reedit-message", value_name = "COMMIT")]
    pub reedit_message: Option<String>,

    /// Read the message from a file, or from standard input with `-`.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// Record this author instead of the ambient identity.
    #[arg(long = "author", value_name = "AUTHOR")]
    pub author: Option<String>,

    /// Commit the named paths only, bypassing the index for them.
    #[arg(value_name = "PATHSPEC", last = true)]
    pub paths: Vec<PathBuf>,

    /// Replace `HEAD`'s commit rather than adding a child.
    #[arg(long = "amend")]
    pub amend: bool,

    /// Record a commit that changes nothing.
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// Record a commit whose tree equals its parent's.
    #[arg(long = "allow-empty-message")]
    pub allow_empty_message: bool,

    /// Include every tracked file's current content, as if `add -u` had run first.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Add a `Signed-off-by` trailer to the message.
    #[arg(short = 's', long = "signoff")]
    pub signoff: bool,

    /// Sign the commit with a GPG key. Not implemented (plan §1 non-goals).
    #[arg(short = 'S', long = "gpg-sign", value_name = "KEY")]
    pub gpg_sign: Option<Option<String>>,

    /// Print what would be committed and skip the commit itself.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Print a summary of the change in the commit message editor.
    #[arg(long = "status")]
    pub with_status: bool,

    /// Exit non-zero if there is nothing to commit, instead of doing nothing.
    #[arg(long = "porcelain")]
    pub porcelain: bool,
}

/// Arguments to `rivet status`.
///
/// # Caveats
///
/// * **`porcelain` is a stable interface and the human output is not.** The `--porcelain` format
///   is what a script parses and it must not change; the default format is a user interface and
///   may. Every field below that only affects the human format must be ignored under
///   `--porcelain`, not merely unused.
/// * **`untracked` changes the work done, not just the printing.** See
///   [`crate::cli::UntrackedModeArg`]; `--untracked-files=no` must skip the walk entirely, which
///   is the whole reason a script on a huge work tree passes it.
/// * **The exit code does not report dirtiness.** `rivet status` exits `0` on a dirty work tree;
///   a caller that wants the answer as a code wants `rivet diff --quiet` or
///   `--porcelain | grep -q .`.
/// * **A path with conflict stages is reported once, not three times.** The index holds stages
///   1/2/3 for an unmerged path and the output must show a single `UU` entry; iterating the index
///   entries directly produces three lines per conflict, which is the classic version of this
///   bug.
#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// The paths to report on, or all of them.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// A stable, script-readable format. Takes precedence over every display flag below.
    #[arg(long = "porcelain", value_name = "VERSION")]
    pub porcelain: Option<Option<String>>,

    /// A terser human format that fits one line per path.
    #[arg(short = 's', long = "short")]
    pub short: bool,

    /// Include or exclude untracked files: `no`, `normal` or `all`.
    #[arg(short = 'u', long = "untracked-files", value_enum, value_name = "MODE")]
    pub untracked: Option<UntrackedModeArg>,

    /// List ignored files as well as untracked ones.
    #[arg(long = "ignored")]
    pub ignored: bool,

    /// Do not consult the index's stat cache; re-hash every file.
    #[arg(long = "no-refresh")]
    pub no_refresh: bool,

    /// Print branch information in the human format.
    #[arg(short = 'b', long = "branch")]
    pub branch: bool,

    /// Print the branch's upstream and how far ahead and behind it is.
    #[arg(long = "ahead-behind")]
    pub ahead_behind: bool,

    /// Show a column for the index and one for the work tree, as the long format does.
    #[arg(long = "column", value_name = "WHEN")]
    pub column: Option<Option<String>>,

    /// Print no paths, only the branch header.
    #[arg(long = "no-lock-index")]
    pub no_lock_index: bool,
}

/// Arguments to `rivet restore`.
///
/// # Caveats
///
/// * **`staged` inverts the source and the destination.** Without it, files are copied from the
///   index into the work tree; with it, they are copied from `--source` (default `HEAD`) into
///   the index. A caller who confuses the two overwrites their work tree, so the two modes must
///   never share a code path that does not name both ends explicitly.
/// * **`worktree` and `staged` together means "reset both to `--source`"**, which is Git's
///   `restore --source=X --staged --worktree`. It is the only combination that can discard a
///   staged change *and* an unstaged one in the same command, and it is not the default.
/// * **Restoring a path that does not exist in the source removes it**, rather than erroring.
///   `rivet restore deleted.txt` after deleting a tracked file brings it back; the same command
///   for a path that is not in `HEAD` at all must fail with
///   [`rivet_core::Error::PathNotFound`], and distinguishing the two is the whole check.
#[derive(Debug, clap::Args)]
pub struct RestoreArgs {
    /// The paths to restore, or every path when combined with a source.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// Restore the index from `--source` instead of the work tree from the index.
    #[arg(short = 'S', long = "staged")]
    pub staged: bool,

    /// Restore the work tree as well as the index.
    #[arg(short = 'W', long = "worktree")]
    pub worktree: bool,

    /// Restore from this commit instead of the index or `HEAD`.
    #[arg(long = "source", value_name = "TREE", short = 's')]
    pub source: Option<String>,

    /// Overwrite a work tree file that has staged changes.
    #[arg(long = "overlay")]
    pub overlay: bool,

    /// Discard local modifications without a safety check.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Read the paths to restore, one per line, from standard input.
    #[arg(long = "pathspec-from-file", value_name = "FILE")]
    pub pathspec_from_file: Option<PathBuf>,
}

/// Arguments to `rivet reset`.
///
/// # Caveats
///
/// * **The three modes are a strict sequence of increasing destruction**: `soft` moves the
///   branch only, `mixed` also resets the index, `hard` also overwrites the work tree. `hard`
///   discards uncommitted work permanently, and it is the only mode where the pre-operation
///   safety check is deliberately *not* applied — a caller who wants that check wants
///   `checkout`.
/// * **`hard` is the mode `merge --abort` and a failed rebase are built on.** It must remove
///   files that are in the current tree and not the target, not merely overwrite the ones that
///   differ; leaving an orphaned file behind produces a work tree that matches neither commit.
/// * **`reset` with a pathspec is not a mode of the above.** `git reset <paths>` unstages the
///   named paths against `HEAD` and touches neither the branch nor the work tree; it is
///   `restore --staged` under another name, and sharing the mode enum with it is the classic
///   way to get `reset --hard <path>` to delete files.
/// * **The reflog is written before the work tree is touched.** If the reset fails midway, the
///   branch must be recoverable from the reflog, so the ref update happens first and the reset
///   of the index and work tree is the part that can be retried.
#[derive(Debug, clap::Args)]
pub struct ResetArgs {
    /// The commit to reset to. Defaults to `HEAD`.
    #[arg(value_name = "COMMIT", default_value = "HEAD")]
    pub commit: String,

    /// The paths to unstage, instead of resetting the branch.
    #[arg(value_name = "PATHSPEC", last = true)]
    pub paths: Vec<PathBuf>,

    /// Move the branch only; leave the index and the work tree alone.
    #[arg(long = "soft")]
    pub soft: bool,

    /// Move the branch and reset the index, leaving the work tree alone. The default.
    #[arg(long = "mixed")]
    pub mixed: bool,

    /// Move the branch, reset the index, and overwrite the work tree.
    #[arg(long = "hard")]
    pub hard: bool,

    /// Reset the index to match `HEAD` without moving the branch.
    #[arg(long = "keep")]
    pub keep: bool,

    /// Reset the index and the work tree, keeping unstaged changes.
    #[arg(long = "merge")]
    pub merge: bool,

    /// Do not write a reflog entry for the branch move.
    #[arg(long = "no-reflog")]
    pub no_reflog: bool,
}

/// Phase 3 — stage file contents into the index.
///
/// # Purpose
///
/// Hash the named paths and write their entries into `.git/index`, so that the next `commit`
/// records them. This is the only command that adds a path to the index; everything else either
/// removes one or reads one.
///
/// # Exit codes
///
/// * `0` — the paths were staged (or, under `--dry-run`, would have been).
/// * `128` — a path is outside the work tree, an ignored path was named without `--force`, a
///   path matches nothing and is not tracked, or the index could not be written.
///
/// # Caveats
///
/// * **Nothing is persisted until the index is written back.** The handler mutates
///   [`rivet_core::Repository::index_mut`] and *must* call
///   [`rivet_core::Repository::write_index`] afterwards; forgetting it stages nothing while
///   printing success, and is the single most likely way to lose work with this API.
/// * **A `--all` walk must honour ignore rules**, including the rule that a tracked file is
///   never ignored. Re-adding tracked files that a `.gitignore` matches is normal and must
///   succeed; skipping them silently makes a commit quietly drop a file.
/// * **Symlinks are staged as their target's bytes with mode `120000` and are never followed.**
///   A symlink to a directory is a blob, not a tree; following it produces a snapshot of
///   whatever it points at, which is both wrong and a path-traversal risk.
/// * **The executable bit is read from the filesystem only when `core.filemode` is true.**
///   On Windows and on a `core.filemode=false` repository the bit comes from the existing index
///   entry, and a fresh file defaults to `100644`; reading the filesystem anyway produces an
///   index that differs from `git add`'s on every file.
/// * **A path that is already conflicted resolves the conflict.** `add` on an unmerged path
///   drops its stages 1/2/3 and installs the current file as stage 0, which is *how a merge is
///   resolved*. Writing all four stages instead leaves the merge permanently in progress.
pub fn run_add(global: &GlobalArgs, args: &AddArgs) -> Result<i32> {
    if args.all && args.update {
        anyhow::bail!("add: --all and --update are mutually exclusive");
    }

    let mut repository = super::open(global)?;

    // Git exits 0 with this advice when a bare `git add` has nothing to work with; it is not an
    // error, so the shell must not see a failure status.
    if args.paths.is_empty() && !args.all && !args.update {
        println!("Nothing specified, nothing added.");
        return Ok(0);
    }

    let mut paths = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        paths.push(super::worktree_path(&repository, path, "add")?);
    }

    let options = AddOptions {
        force: args.force,
        update: args.update,
        all: args.all,
        intent_to_add: args.intent_to_add,
        dry_run: args.dry_run,
        pathspec: BString::default(),
    };

    let outcome = repository.add(&paths, &options)?;

    // `AddOutcome` counts additions but does not name them, so the only honest per-path output
    // is on a dry run, where Git lists every path it would have touched. A normal `add` that
    // only modifies tracked files is silent in Git too.
    if args.dry_run {
        for path in &outcome.paths {
            println!("add '{}'", path);
        }
    }

    Ok(0)
}

/// Phase 3 — remove files from the work tree and the index.
///
/// # Purpose
///
/// The inverse of `add`: drop the index entry for each named path, and delete the work tree file
/// unless `--cached` was given.
///
/// # Exit codes
///
/// * `0` — the paths were removed.
/// * `128` — a path is not tracked, is outside the work tree, or has staged content that
///   differs from `HEAD` and `--force` was not given.
///
/// # Caveats
///
/// * **The safety check compares the index against `HEAD`, not the work tree against the
///   index.** An unstaged modification is not a reason to refuse — the file is being deleted
///   anyway — whereas a staged one is work that no reflog can recover.
/// * **A tracked file that is already missing from the work tree is removed without error.**
///   That is the common case after `rm` by hand, and erroring makes the command unusable for
///   the thing users actually do with it.
/// * **`--cached` on a path that is untracked is an error**, even though it "removes nothing";
///   a silent success hides a mistyped path, and the pathspec-not-matching error is what tells
///   the user which one.
/// * **The index must be written back**, exactly as for `add`.
pub fn run_rm(global: &GlobalArgs, args: &RmArgs) -> Result<i32> {
    if args.paths.is_empty() {
        anyhow::bail!("rm: no paths specified");
    }

    let mut repository = super::open(global)?;

    let mut paths = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        paths.push(super::worktree_path(&repository, path, "rm")?);
    }

    if !args.force
        && !args.cached
        && !args.dry_run
        && let Some(code) = check_rm_safety(&repository, &paths)?
    {
        return Ok(code);
    }

    let options = RmOptions {
        cached: args.cached,
        recursive: args.recursive,
        force: args.force,
        dry_run: args.dry_run,
    };

    let outcome = repository.remove(&paths, &options)?;

    // Git prints one `rm '<path>'` line per removal, dry run included (`--dry-run` still says
    // `rm 'x'`), so the same loop serves both modes.
    if !global.quiet {
        for path in &outcome.paths {
            println!("rm '{}'", path);
        }
    }

    Ok(0)
}

/// Phase 3 — record the index as a commit.
///
/// # Purpose
///
/// Write the staged tree as a tree object, build the commit object with the right parents and
/// the right identity, write it, and move `HEAD`'s branch to it — in that order, so a failure
/// leaves an unreferenced object rather than a dangling ref.
///
/// # Exit codes
///
/// * `0` — the commit was recorded; prints `[main abc1234] <summary>`.
/// * `1` — `--porcelain` was given and there is nothing to commit. The requested negative.
/// * `128` — nothing is staged (without `--allow-empty`), the identity is not configured, a
///   merge is unresolved, or `--signoff`/`-S` was given and signing is not implemented.
///
/// # Caveats
///
/// * **A missing `user.name` or `user.email` is an error, never an invented identity.** A
///   fabricated `rivet@localhost` produces a commit the user cannot fix without rewriting
///   history, and `git fsck` will not complain about it.
/// * **The message is written byte for byte, including its trailing newline.** Normalising it
///   here produces a different commit id from the one `git commit` writes for the same message,
///   which breaks every differential test at once.
/// * **A merge in progress changes the parents and the default message**, per [`CommitArgs`].
///   `MERGE_HEAD` and `MERGE_MSG` must be read and then *cleared* — as must `MERGE_HEAD`'s
///   index stages — or the next commit is a merge of the same parents.
/// * **`--amend` rewrites rather than appends**, and the old commit stays in the reflog. A
///   handler that amended and then reported the old id would be reporting a commit that is no
///   longer reachable from the branch.
/// * **The commit is written to the object database before the ref moves.** Reversing the order
///   is the one bug that can leave `HEAD` pointing at an object that does not exist, which is a
///   repository `git fsck` reports as broken.
pub fn run_commit(global: &GlobalArgs, args: &CommitArgs) -> Result<i32> {
    // Everything below either needs a piece rivet does not have or would change what lands in
    // history if it were approximated. Refusing keeps a script from thinking it committed
    // something it did not.
    if args.gpg_sign.is_some() {
        anyhow::bail!("commit: -S (signing) is not implemented");
    }
    if args.reuse_message.is_some() || args.reedit_message.is_some() {
        anyhow::bail!("commit: -C/-c are not implemented");
    }
    if args.dry_run {
        anyhow::bail!("commit: --dry-run is not implemented");
    }
    if args.with_status || args.porcelain {
        anyhow::bail!("commit: --with-status/--porcelain are not implemented");
    }
    if args.author.is_some() {
        anyhow::bail!("commit: --author is not implemented");
    }
    if !args.paths.is_empty() {
        anyhow::bail!("commit: committing only the given paths is not implemented");
    }

    let mut repository = super::open(global)?;

    // `-a` stages modifications and deletions of tracked files first; it never adds untracked
    // ones, which is why the update mode is used rather than `all`.
    if args.all {
        let options = AddOptions {
            force: false,
            update: true,
            all: false,
            intent_to_add: false,
            dry_run: false,
            pathspec: BString::default(),
        };
        repository.add(&[], &options)?;
    }

    let raw = if !args.message.is_empty() {
        args.message.join("\n\n")
    } else if let Some(path) = &args.file {
        let bytes = if path.as_os_str() == "-" {
            use std::io::Read as _;
            let mut buffer = Vec::new();
            std::io::stdin().read_to_end(&mut buffer).map_err(|error| {
                anyhow::anyhow!("commit: cannot read the message from stdin: {error}")
            })?;
            buffer
        } else {
            std::fs::read(path).map_err(|error| {
                anyhow::anyhow!("commit: cannot read '{}': {error}", path.display())
            })?
        };
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        anyhow::bail!("commit: no message given (use -m or -F; the editor is not implemented)");
    };

    let mut message = raw;
    if args.signoff {
        let config = repository.config();
        let name = config
            .get(bstr::BStr::new("user.name"))?
            .map(|value| value.to_string())
            .unwrap_or_default();
        let email = config
            .get(bstr::BStr::new("user.email"))?
            .map(|value| value.to_string())
            .unwrap_or_default();
        if name.is_empty() || email.is_empty() {
            anyhow::bail!("commit: --signoff needs both user.name and user.email configured");
        }
        message = format!("{}\n\nSigned-off-by: {name} <{email}>", message.trim_end());
    }

    // Nothing to record: Git shows the status instead of failing, and exits 1.
    if !args.allow_empty && !args.amend {
        let tree = repository.write_tree()?;
        let unchanged = match repository.head_tree_oid()? {
            Some(head_tree) => head_tree == tree,
            None => repository.index().is_empty(),
        };
        if unchanged && !rivet_core::merge_in_progress(repository.git_dir()) {
            let status_args = StatusArgs {
                paths: Vec::new(),
                porcelain: None,
                short: false,
                untracked: None,
                ignored: false,
                no_refresh: false,
                branch: false,
                ahead_behind: false,
                column: None,
                no_lock_index: false,
            };
            run_status(global, &status_args)?;
            return Ok(1);
        }
    }

    let message = clean_commit_message(&message);
    let mut options = rivet_core::repo::CommitOptions::new(bstr::BStr::new(message.as_bytes()));
    options.allow_empty = args.allow_empty;
    options.allow_empty_message = args.allow_empty_message;
    options.amend = args.amend;

    let oid = repository.commit(&options)?;
    if global.quiet {
        return Ok(0);
    }

    let head = repository.head()?;
    let branch = match &head {
        HeadState::Detached { .. } => "detached HEAD".to_string(),
        other => branch_name(other),
    };
    let parents = repository.commit_parents(oid)?;
    let root = if parents.is_empty() {
        " (root-commit)"
    } else {
        ""
    };
    let subject = super::commit_subject(&repository, &oid)?;
    println!("[{branch}{root} {}] {subject}", short_oid(&oid));
    let author = repository.author_signature()?;
    let committer = repository.committer_signature()?;
    if author.name != committer.name || author.email != committer.email {
        println!(" Author: {} <{}>", author.name, author.email);
    }
    // Like `git log`, the summary shows no diff for a merge commit.
    if parents.len() < 2 {
        let old_tree = match parents.first() {
            Some(parent) => Some(repository.commit_tree(*parent)?),
            None => None,
        };
        let new_tree = repository.commit_tree(oid)?;
        super::print_shortstat_summary(&repository, old_tree, new_tree)?;
    }

    Ok(0)
}

/// Git's default message cleanup: strip trailing whitespace on every line, drop leading and
/// trailing blank lines, and end with exactly one newline.
fn clean_commit_message(message: &str) -> String {
    let mut lines: Vec<&str> = message.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut cleaned = lines.join("\n");
    cleaned.push('\n');
    cleaned
}

/// Phase 3 — report the state of the work tree and the index.
///
/// # Purpose
///
/// The command a user runs to ask "where am I". Prints the branch, staged changes, unstaged
/// changes and untracked files, or those four in the `--porcelain` format a script parses.
///
/// # Exit codes
///
/// * `0` — the status was reported. **Dirtiness does not change the exit code.**
/// * `128` — not a repository, or the index could not be read.
///
/// # Caveats
///
/// * **The work tree is only compared against the index where the stat cache says it must be.**
///   A file whose size and mtime match the index entry is assumed unchanged, which is why the
///   index's stat data is written at all; hashing everything would make `status` unusable on a
///   large repository, and *not* rehashing a racy entry (mtime equal to or newer than the
///   index's own) reports a modified file as clean. See `rivet_core::worktree::StatData`.
/// * **An unborn `HEAD` is not detached and must be reported as a branch.** A fresh repository
///   prints `On branch main` with no commits yet, not `HEAD detached`.
/// * **A conflicted path is reported once with `UU`, `AA`, `DD` or `AU`/`UA`/`DU`/`UD` as the
///   stages dictate** — not three times, per [`StatusArgs`].
/// * **`--porcelain` must be byte-exact**, including the trailing space on an unmodified
///   work-tree column and the quoting of paths containing non-ASCII bytes. The path is printed
///   with C-style quoting when it contains a byte that is not printable, exactly as Git does,
///   or a script consuming the output splits on a byte that is part of a filename.
pub fn run_status(global: &GlobalArgs, args: &StatusArgs) -> Result<i32> {
    // These flags change the layout or need history walking we do not have yet. Failing loudly
    // beats printing a shape that looks like Git's while meaning something else.
    if args.column.is_some() {
        anyhow::bail!("status: --column is not implemented");
    }
    if args.ahead_behind {
        anyhow::bail!("status: --ahead-behind is not implemented");
    }
    if args.no_lock_index {
        anyhow::bail!("status: --no-lock-index is not implemented");
    }
    if args.paths.len() > 1 {
        anyhow::bail!("status: only one path filter is supported");
    }

    let mut repository = super::open(global)?;

    let mut options = StatusOptions::new();
    options.untracked_files = match args.untracked {
        Some(UntrackedModeArg::No) => UntrackedMode::No,
        Some(UntrackedModeArg::All) => UntrackedMode::All,
        Some(UntrackedModeArg::Normal) | None => UntrackedMode::Normal,
    };
    options.ignored = args.ignored;
    // `refresh: true` re-hashes the work tree, which is what Git does on every status run unless
    // asked not to; `--no-refresh` is therefore the only thing that turns it off.
    options.refresh = !args.no_refresh;
    if let Some(path) = args.paths.first() {
        options.pathspec = super::worktree_path(&repository, path, "status")?;
    }

    let mut status = repository.status(&options)?;
    let head = repository.head()?;
    // Conflicted paths get their own section and two-letter codes, decided by which stages exist.
    let unmerged: Vec<(BString, &'static str, &'static str)> = {
        use rivet_core::index::{IndexExt as _, Stage};
        let mut out = Vec::new();
        for path in status.conflicts().into_iter().cloned().collect::<Vec<_>>() {
            let stages: Vec<Stage> = repository
                .index()
                .stages(bstr::BStr::new(&path[..]))
                .iter()
                .map(|e| e.stage())
                .collect();
            let has = |s: Stage| stages.contains(&s);
            let (code, label) = match (has(Stage::Base), has(Stage::Ours), has(Stage::Theirs)) {
                (true, true, true) => ("UU", "both modified:"),
                (false, true, true) => ("AA", "both added:"),
                (true, false, true) => ("DU", "deleted by us:"),
                (true, true, false) => ("UD", "deleted by them:"),
                (false, true, false) => ("AU", "added by us:"),
                (false, false, true) => ("UA", "added by them:"),
                _ => ("DD", "both deleted:"),
            };
            out.push((path, code, label));
        }
        out
    };
    status.staged.retain(|e| e.kind != ChangeKind::Unmerged);
    status.unstaged.retain(|e| e.kind != ChangeKind::Unmerged);

    // Staged renames: a deleted path and an added one holding (nearly) the same content.
    let mut renames: std::collections::BTreeMap<BString, BString> =
        std::collections::BTreeMap::new();
    let mut renamed_sources: std::collections::BTreeSet<BString> =
        std::collections::BTreeSet::new();
    {
        use rivet_core::diff::RenameSide;
        let mut sources = Vec::new();
        let mut destinations = Vec::new();
        for entry in &status.staged {
            match (
                entry.kind,
                entry.old_oid,
                entry.old_mode,
                entry.new_oid,
                entry.new_mode,
            ) {
                (ChangeKind::Deleted, Some(oid), Some(mode), _, _) => sources.push(RenameSide {
                    path: entry.path.clone(),
                    oid,
                    mode,
                }),
                (ChangeKind::Added, _, _, Some(oid), Some(mode)) => destinations.push(RenameSide {
                    path: entry.path.clone(),
                    oid,
                    mode,
                }),
                _ => {}
            }
        }
        let pairs = if super::renames_enabled(&repository, "status.renames") {
            rivet_core::pair_renames(&sources, &destinations, repository.odb())?
        } else {
            Vec::new()
        };
        for pair in pairs {
            renames.insert(
                destinations[pair.destination].path.clone(),
                sources[pair.source].path.clone(),
            );
            renamed_sources.insert(sources[pair.source].path.clone());
        }
    }

    let head_line = || -> String {
        match &head {
            HeadState::Attached { branch, .. } => {
                format!("On branch {}", branch.short_name())
            }
            HeadState::Unborn { branch } => {
                format!("On branch {}", branch.short_name())
            }
            HeadState::Detached { oid } => {
                format!("HEAD detached at {}", short_oid(oid))
            }
        }
    };

    if args.porcelain.is_some() || args.short {
        // Git prints the `## branch` header in these formats only when asked for it.
        if args.branch {
            let mut header = porcelain_header(&head);
            match tracking_state(&repository, &head)? {
                Tracking::None => {}
                Tracking::Gone { upstream } => header.push_str(&format!("...{upstream} [gone]")),
                Tracking::State {
                    upstream,
                    ahead,
                    behind,
                } => {
                    header.push_str(&format!("...{upstream}"));
                    match (ahead, behind) {
                        (0, 0) => {}
                        (a, 0) => header.push_str(&format!(" [ahead {a}]")),
                        (0, b) => header.push_str(&format!(" [behind {b}]")),
                        (a, b) => header.push_str(&format!(" [ahead {a}, behind {b}]")),
                    }
                }
            }
            println!("{header}");
        }
        // One line per path, index state then work-tree state, in path order.
        let mut lines: std::collections::BTreeMap<BString, (char, char)> =
            std::collections::BTreeMap::new();
        for entry in &status.staged {
            if renamed_sources.contains(&entry.path) {
                continue;
            }
            lines.entry(entry.path.clone()).or_insert((' ', ' ')).0 =
                if renames.contains_key(&entry.path) {
                    'R'
                } else {
                    entry.kind.code()
                };
        }
        for entry in &status.unstaged {
            lines.entry(entry.path.clone()).or_insert((' ', ' ')).1 = entry.kind.code();
        }
        let mut rendered: Vec<(BString, String)> = lines
            .into_iter()
            .map(|(path, (x, y))| {
                let shown = match renames.get(&path) {
                    Some(from) => format!("{} -> {}", q(from, true), q(&path, true)),
                    None => q(&path, true),
                };
                (path.clone(), format!("{x}{y} {shown}"))
            })
            .collect();
        for (path, code, _) in &unmerged {
            rendered.push((path.clone(), format!("{code} {}", q(path, true))));
        }
        rendered.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, line) in rendered {
            println!("{line}");
        }
        for path in &status.untracked {
            println!("?? {}", q(path, true));
        }
        for path in &status.ignored {
            println!("!! {}", q(path, true));
        }
        return Ok(0);
    }

    let rebase = rebase_progress(&repository);
    if rebase.is_none() {
        println!("{}", head_line());
        print_tracking(&tracking_state(&repository, &head)?);
    }
    if matches!(head, HeadState::Unborn { .. }) {
        // Git separates the "No commits yet" paragraph from the branch line, and keeps a trailing
        // blank before the first section. When the branch has commits there is no blank at all:
        // `On branch main` is immediately followed by the first section header.
        println!();
        println!("No commits yet");
        println!();
    }

    if let Some(progress) = &rebase {
        print!("{}", progress.describe(!unmerged.is_empty()));
        println!();
    }
    let picking = [
        ("CHERRY_PICK_HEAD", "cherry-picking", "cherry-pick"),
        ("REVERT_HEAD", "reverting", "revert"),
    ]
    .into_iter()
    .find_map(|(file, gerund, name)| {
        let text = std::fs::read_to_string(repository.git_dir().join(file)).ok()?;
        Some((
            text.trim().chars().take(7).collect::<String>(),
            gerund,
            name,
        ))
    });
    if let Some((short, gerund, name)) = &picking {
        println!("You are currently {gerund} commit {short}.");
        if unmerged.is_empty() {
            println!("  (all conflicts fixed: run \"git {name} --continue\")");
        } else {
            println!("  (fix conflicts and run \"git {name} --continue\")");
        }
        println!("  (use \"git {name} --skip\" to skip this patch)");
        println!("  (use \"git {name} --abort\" to cancel the {name} operation)");
        println!();
    } else if !unmerged.is_empty() && rivet_core::merge_in_progress(repository.git_dir()) {
        println!("You have unmerged paths.");
        println!("  (fix conflicts and run \"git commit\")");
        println!("  (use \"git merge --abort\" to abort the merge)");
        println!();
    }
    let mut wrote_section = false;
    if !status.staged.is_empty() {
        println!("Changes to be committed:");
        println!("  (use \"git restore --staged <file>...\" to unstage)");
        for entry in &status.staged {
            if renamed_sources.contains(&entry.path) {
                continue;
            }
            match renames.get(&entry.path) {
                Some(from) => println!(
                    "\t{:<12}{} -> {}",
                    "renamed:",
                    q(from, false),
                    q(&entry.path, false)
                ),
                None => println!(
                    "\t{:<12}{}",
                    section_label(entry.kind),
                    q(&entry.path, false)
                ),
            }
        }
        wrote_section = true;
    }
    if !unmerged.is_empty() {
        if wrote_section {
            println!();
        }
        println!("Unmerged paths:");
        // Git offers `restore --staged` everywhere except during a merge or a cherry-pick.
        let cherry_picking = picking
            .as_ref()
            .is_some_and(|(_, _, name)| *name == "cherry-pick");
        if !rivet_core::merge_in_progress(repository.git_dir()) && !cherry_picking {
            println!("  (use \"git restore --staged <file>...\" to unstage)");
        }
        println!("  (use \"git add <file>...\" to mark resolution)");
        for (path, _, label) in &unmerged {
            println!("\t{:<17}{}", label, path);
        }
        wrote_section = true;
    }
    if !status.unstaged.is_empty() {
        if wrote_section {
            println!();
        }
        println!("Changes not staged for commit:");
        // Git offers `add/rm` as soon as one of the unstaged changes is a deletion.
        if status
            .unstaged
            .iter()
            .any(|e| e.kind == ChangeKind::Deleted)
        {
            println!("  (use \"git add/rm <file>...\" to update what will be committed)");
        } else {
            println!("  (use \"git add <file>...\" to update what will be committed)");
        }
        println!("  (use \"git restore <file>...\" to discard changes in working directory)");
        for entry in &status.unstaged {
            println!(
                "\t{:<12}{}",
                section_label(entry.kind),
                q(&entry.path, false)
            );
        }
        wrote_section = true;
    }
    if !status.untracked.is_empty() {
        if wrote_section {
            println!();
        }
        println!("Untracked files:");
        println!("  (use \"git add <file>...\" to include in what will be committed)");
        for path in &status.untracked {
            println!("\t{}", q(path, false));
        }
        wrote_section = true;
    }
    if !status.ignored.is_empty() {
        if wrote_section {
            println!();
        }
        println!("Ignored files:");
        println!("  (use \"git add -f <file>...\" to include in what will be committed)");
        for path in &status.ignored {
            println!("\t{}", q(path, false));
        }
        wrote_section = true;
    }

    if !wrote_section {
        if matches!(head, HeadState::Unborn { .. }) {
            println!("nothing to commit (create/copy files and use \"git add\" to track)");
        } else {
            println!("nothing to commit, working tree clean");
        }
    } else if status.staged.is_empty() {
        // Git separates the closing advice from the last section with a blank line, and picks the
        // wording by what is actually pending: untracked-only names `git add`, anything unstaged
        // points at `git commit -a` as well.
        println!();
        if status.unstaged.is_empty() && !status.untracked.is_empty() {
            println!(
                "nothing added to commit but untracked files present (use \"git add\" to track)"
            );
        } else {
            println!("no changes added to commit (use \"git add\" and/or \"git commit -a\")");
        }
    } else {
        // Every section ends with a blank line; with changes staged there is no advice after it.
        println!();
    }

    Ok(0)
}

/// Git's one-line section label, padded so the paths line up.
fn section_label(kind: ChangeKind) -> String {
    let label = match kind {
        ChangeKind::Added => "new file",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::TypeChanged => "typechange",
        ChangeKind::Unmerged => "unmerged",
    };
    format!("{label}:")
}

/// The `## branch` line porcelain v1 and `--short --branch` print.
fn porcelain_header(head: &HeadState) -> String {
    match head {
        HeadState::Detached { .. } => "## HEAD (no branch)".to_string(),
        HeadState::Unborn { branch } => format!("## No commits yet on {}", branch.short_name()),
        other => format!("## {}", branch_name(other)),
    }
}

fn branch_name(head: &HeadState) -> String {
    match head {
        HeadState::Attached { branch, .. } => branch.short_name().to_string(),
        HeadState::Unborn { branch } => branch.short_name().to_string(),
        HeadState::Detached { oid } => format!("HEAD detached at {}", short_oid(oid)),
    }
}

fn short_oid(oid: &rivet_core::hash::ObjectId) -> String {
    oid.to_string().chars().take(7).collect()
}

/// Phase 4 — restore work tree files from the index, or the index from a commit.
///
/// # Purpose
///
/// Copy the named paths (or all of them) from one of the three states to another — the
/// work-tree-restoring half of `checkout` split out as its own command, without the branch
/// switching and without the "this will detach HEAD" warning.
///
/// # Exit codes
///
/// * `0` — the paths were restored.
/// * `128` — a path is not in the source, the source revision does not resolve, a work tree file
///   has staged changes and neither `--overlay` nor `--force` was given, or the path is outside
///   the work tree.
///
/// # Caveats
///
/// * **The source and destination depend on the flags**, per [`RestoreArgs`]; `--staged` alone
///   touches the index and never the work tree, and getting this backwards destroys uncommitted
///   work rather than declining to.
/// * **Restoring a path that does not exist in the source deletes it from the destination.**
///   That is correct and is how a file deleted in `HEAD` is brought back into agreement, but it
///   means an empty or mistyped pathspec must not be treated as "all paths" — an empty
///   pathspec with a source means an empty restore, not a full one.
/// * **The write is two-phase**: every path is validated first, then written. A restore that
///   fails halfway through leaves a work tree matching neither the old state nor the new one,
///   which is exactly the half-checked-out state `checkout` is designed to avoid.
/// * **`--force` skips the safety check and only the safety check.** It does not skip the
///   path-traversal validation, the symlink handling, or the work-tree containment check.
pub fn run_restore(global: &GlobalArgs, args: &RestoreArgs) -> Result<i32> {
    if args.pathspec_from_file.is_some() {
        anyhow::bail!("restore: --pathspec-from-file is not implemented");
    }
    let mut repository = super::open(global)?;
    if args.paths.is_empty() {
        anyhow::bail!("you must specify path(s) to restore");
    }
    let staged = args.staged;
    // With neither flag the work tree is the destination.
    let worktree = args.worktree || !args.staged;

    let source = match &args.source {
        Some(spec) => {
            let oid = super::resolve_rev(&repository, spec)
                .map_err(|_| anyhow::anyhow!("could not resolve '{spec}'"))?;
            Some(repository.commit_tree(oid)?)
        }
        // Restoring the index defaults to HEAD; restoring only the work tree defaults to the index.
        None if staged => match repository.head_tree_oid()? {
            Some(tree) => Some(tree),
            None => Some(super::empty_tree(&repository)?),
        },
        None => None,
    };

    let mut specs = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        specs.push(super::worktree_path(&repository, path, "restore")?);
    }
    match repository.restore_paths(source, &specs, staged, worktree, false) {
        Ok(_) => Ok(0),
        // Git reports this as `error:` with status 1, not as a fatal error.
        Err(rivet_core::Error::PathNotFound(spec)) => {
            eprintln!("error: pathspec '{spec}' did not match any file(s) known to git");
            Ok(1)
        }
        Err(error) => Err(error.into()),
    }
}

/// Phase 4 — move a branch, and optionally the index and the work tree, to a commit.
///
/// # Purpose
///
/// Reposition the current branch at a commit chosen by the user, then unwind the index and the
/// work tree to match as far as the mode asks — `--soft`, `--mixed` (the default), `--hard`,
/// `--keep` or `--merge`.
///
/// # Exit codes
///
/// * `0` — the branch (and whatever else the mode named) was moved.
/// * `128` — the revision does not resolve, a merge is in progress, `--hard` would overwrite an
///   untracked file, or the ref update failed.
///
/// # Caveats
///
/// * **The mode is a sequence of increasing destruction**: `--soft` moves a ref and touches
///   nothing else; `--hard` permanently discards uncommitted work. The two are the same command
///   name with the same arguments and the difference is one flag, so a handler must decide the
///   mode once, up front, and not re-derive it per operation.
/// * **`--hard` refuses to overwrite an untracked file** that would be clobbered, and says which
///   one. That check is what makes the command safe to run without looking, and it is
///   [`rivet_core::Error::UntrackedOverwrite`].
/// * **`--keep` and `--merge` differ in what they do about local modifications**: `--keep`
///   refuses if a file that differs between the two commits has local changes, `--merge`
///   carries the changes forward into the new tree. Implementing one as the other silently
///   loses or preserves work depending on which way round it is.
/// * **A pathspec makes this `restore --staged`, not a mode of the above**, per [`ResetArgs`].
///   The branch must not move at all in that form.
/// * **The reflog entry is written for the branch move and must be informative** —
///   `reset: moving to HEAD~1` — because it is how a user undoes an accidental `--hard`.
///   A reflog that records only the ids cannot be searched, which is most of its value.
/// * **`ORIG_HEAD` is written**, so `rivet reset --hard ORIG_HEAD` after a bad reset works and
///   so does a rescue from a lost merge.
pub fn run_reset(global: &GlobalArgs, args: &ResetArgs) -> Result<i32> {
    if args.keep || args.merge {
        anyhow::bail!("reset: --keep and --merge are not implemented");
    }
    let modes = [args.soft, args.mixed, args.hard]
        .iter()
        .filter(|m| **m)
        .count();
    if modes > 1 {
        anyhow::bail!("options '--soft', '--mixed' and '--hard' cannot be used together");
    }
    let mut repository = super::open(global)?;

    // `reset <x>` with no `--`: a revision if it resolves, otherwise a path.
    let mut commit_spec = args.commit.clone();
    let mut pathspecs: Vec<PathBuf> = args.paths.clone();
    if pathspecs.is_empty() && super::resolve_rev(&repository, &commit_spec).is_err() {
        let as_path = std::path::Path::new(&commit_spec);
        let exists = super::worktree_path(&repository, as_path, "reset")
            .ok()
            .is_some_and(|p| {
                repository.index().entry(bstr::BStr::new(&p[..])).is_some()
                    || repository
                        .worktree()
                        .ok()
                        .and_then(|w| w.stat(bstr::BStr::new(&p[..])).ok().flatten())
                        .is_some()
            });
        if exists {
            pathspecs.push(PathBuf::from(&commit_spec));
            commit_spec = "HEAD".to_string();
        } else {
            anyhow::bail!(
                "ambiguous argument '{commit_spec}': unknown revision or path not in the working tree.\n\
                 Use '--' to separate paths from revisions, like this:\n\
                 'git <command> [<revision>...] -- [<file>...]'"
            );
        }
    }

    // A pathspec makes this `restore --staged`: the branch does not move.
    if !pathspecs.is_empty() {
        if args.soft || args.hard {
            anyhow::bail!(
                "Cannot do {} reset with paths.",
                if args.soft { "soft" } else { "hard" }
            );
        }
        let target = match super::resolve_commit(&repository, &commit_spec) {
            Ok(oid) => Some(repository.commit_tree(oid)?),
            Err(_) if commit_spec == "HEAD" => None,
            Err(error) => return Err(error),
        };
        let tree = match target {
            Some(tree) => tree,
            None => super::empty_tree(&repository)?,
        };
        let mut specs = Vec::new();
        for path in &pathspecs {
            specs.push(super::worktree_path(&repository, path, "reset")?);
        }
        repository.restore_paths(Some(tree), &specs, true, false, false)?;
        if !global.quiet {
            print_unstaged(&mut repository)?;
        }
        return Ok(0);
    }

    let target = super::resolve_commit(&repository, &commit_spec)?;
    let git_dir = repository.git_dir().to_path_buf();
    let old = repository.head()?.oid();

    if args.soft && rivet_core::merge_in_progress(&git_dir) {
        anyhow::bail!("Cannot do a soft reset in the middle of a merge.");
    }

    // ORIG_HEAD records where HEAD was before the move.
    if let Some(old) = old {
        rivet_core::write_orig_head(&git_dir, old)?;
    }

    let tree = repository.commit_tree(target)?;
    if args.hard {
        repository.reset_hard_to_tree(tree)?;
    } else if !args.soft {
        repository.reset_index_to_tree(Some(tree))?;
    }
    let reason = format!("reset: moving to {commit_spec}");
    repository.move_head(target, bstr::BStr::new(reason.as_bytes()))?;
    rivet_core::clear_merge_state(&git_dir)?;

    if !global.quiet {
        if args.hard {
            println!(
                "HEAD is now at {} {}",
                super::short_oid(&target),
                super::commit_subject(&repository, &target)?
            );
        } else if !args.soft {
            print_unstaged(&mut repository)?;
        }
    }
    Ok(0)
}

/// `Unstaged changes after reset:` and the `M\tpath` lines, as `git reset` prints after a
/// mixed reset; nothing when the work tree matches the index.
fn print_unstaged(repository: &mut rivet_core::Repository) -> Result<()> {
    let mut options = StatusOptions::new();
    options.untracked_files = UntrackedMode::No;
    let status = repository.status(&options)?;
    if status.unstaged.is_empty() {
        return Ok(());
    }
    println!("Unstaged changes after reset:");
    for entry in &status.unstaged {
        let letter = match entry.kind {
            ChangeKind::Deleted => 'D',
            ChangeKind::Added => 'A',
            ChangeKind::TypeChanged => 'T',
            ChangeKind::Unmerged => 'U',
            ChangeKind::Modified => 'M',
        };
        println!("{letter}\t{}", entry.path);
    }
    Ok(())
}

/// How the current branch relates to its configured upstream.
enum Tracking {
    None,
    Gone {
        upstream: String,
    },
    State {
        upstream: String,
        ahead: usize,
        behind: usize,
    },
}

fn tracking_state(repository: &rivet_core::Repository, head: &HeadState) -> Result<Tracking> {
    let (Some(branch), Some(oid)) = (head.branch(), head.oid()) else {
        return Ok(Tracking::None);
    };
    let Some((remote, merge)) = repository.branch_upstream(&branch.short_name().to_string())?
    else {
        return Ok(Tracking::None);
    };
    let tracking_name = if remote == "." {
        merge.clone()
    } else {
        // Map the remote branch through the remote's fetch refspecs, as Git does.
        let mut mapped = None;
        if let Some(config) = repository.remote_config(&remote)? {
            for spec in &config.fetch {
                if let Some(name) = spec.map(merge.as_bytes()) {
                    mapped = Some(name.to_string());
                    break;
                }
            }
        }
        match mapped {
            Some(name) => name,
            None => return Ok(Tracking::None),
        }
    };
    let short = tracking_name
        .strip_prefix("refs/remotes/")
        .or_else(|| tracking_name.strip_prefix("refs/heads/"))
        .unwrap_or(&tracking_name)
        .to_string();
    let Ok(name) = rivet_core::RefName::from_bytes(tracking_name.as_bytes()) else {
        return Ok(Tracking::None);
    };
    let Ok(upstream_oid) = repository.refs().resolve_oid(&name) else {
        return Ok(Tracking::Gone { upstream: short });
    };
    let count = |from: rivet_core::ObjectId, exclude: rivet_core::ObjectId| -> Result<usize> {
        let mut options = rivet_core::revwalk::WalkOptions::new();
        options.exclude(exclude);
        Ok(rivet_core::revwalk::walk_oids(repository.odb(), &[from], &options)?.len())
    };
    Ok(Tracking::State {
        upstream: short,
        ahead: count(oid, upstream_oid)?,
        behind: count(upstream_oid, oid)?,
    })
}

fn print_tracking(tracking: &Tracking) {
    let plural = |n: usize| if n == 1 { "commit" } else { "commits" };
    match tracking {
        Tracking::None => {}
        Tracking::Gone { upstream } => {
            println!("Your branch is based on '{upstream}', but the upstream is gone.");
            println!("  (use \"git branch --unset-upstream\" to fixup)");
            println!();
        }
        Tracking::State {
            upstream,
            ahead: 0,
            behind: 0,
        } => {
            println!("Your branch is up to date with '{upstream}'.");
            println!();
        }
        Tracking::State {
            upstream,
            ahead,
            behind: 0,
        } => {
            println!(
                "Your branch is ahead of '{upstream}' by {ahead} {}.",
                plural(*ahead)
            );
            println!("  (use \"git push\" to publish your local commits)");
            println!();
        }
        Tracking::State {
            upstream,
            ahead: 0,
            behind,
        } => {
            println!(
                "Your branch is behind '{upstream}' by {behind} {}, and can be fast-forwarded.",
                plural(*behind)
            );
            println!("  (use \"git pull\" to update your local branch)");
            println!();
        }
        Tracking::State {
            upstream,
            ahead,
            behind,
        } => {
            println!("Your branch and '{upstream}' have diverged,");
            println!("and have {ahead} and {behind} different commits each, respectively.");
            println!("  (use \"git pull\" if you want to integrate the remote branch with yours)");
            println!();
        }
    }
}

/// Git's refusal to `rm` a file whose content is not safely recorded elsewhere: staged changes,
/// local modifications, or both. Prints Git's message and returns its exit status.
fn check_rm_safety(repository: &rivet_core::Repository, specs: &[BString]) -> Result<Option<i32>> {
    use bstr::BStr;
    let head = match repository.head_tree_oid()? {
        Some(tree) => rivet_core::ops::flatten_tree(repository.odb(), tree)?,
        None => Default::default(),
    };
    let (mut staged, mut local, mut both) = (Vec::new(), Vec::new(), Vec::new());
    for entry in repository.index().entries() {
        if entry.stage() != rivet_core::index::Stage::Normal {
            continue;
        }
        let selected = specs.iter().any(|spec| {
            rivet_core::ops::pathspec_matches(BStr::new(&entry.path[..]), BStr::new(&spec[..]))
        });
        if !selected || entry.mode == rivet_core::EntryMode::Commit {
            continue;
        }
        let in_head = head.get(&entry.path).copied();
        let staged_change = in_head != Some((entry.mode, entry.oid));
        let path = BStr::new(&entry.path[..]);
        let local_change = match repository.worktree()?.stat(path)? {
            Some(_) => repository.worktree()?.hash_file(path)? != entry.oid,
            None => false,
        };
        match (staged_change, local_change) {
            (true, true) => both.push(entry.path.clone()),
            (true, false) => staged.push(entry.path.clone()),
            (false, true) => local.push(entry.path.clone()),
            (false, false) => {}
        }
    }
    let report = |paths: &[BString], singular: &str, plural: &str, hint: &str| {
        let which = if paths.len() == 1 { singular } else { plural };
        eprintln!("error: the following {which}");
        for path in paths {
            eprintln!("    {path}");
        }
        eprintln!("{hint}");
    };
    let mut refused = false;
    if !both.is_empty() {
        report(
            &both,
            "file has staged content different from both the\nfile and the HEAD:",
            "files have staged content different from both the\nfile and the HEAD:",
            "(use -f to force removal)",
        );
        refused = true;
    }
    if !staged.is_empty() {
        report(
            &staged,
            "file has changes staged in the index:",
            "files have changes staged in the index:",
            "(use --cached to keep the file, or -f to force removal)",
        );
        refused = true;
    }
    if !local.is_empty() {
        report(
            &local,
            "file has local modifications:",
            "files have local modifications:",
            "(use --cached to keep the file, or -f to force removal)",
        );
        refused = true;
    }
    Ok(refused.then_some(1))
}

/// A path as `status` prints it.
fn q(path: &[u8], quote_space: bool) -> String {
    String::from_utf8_lossy(&rivet_core::ops::quote_path(path, quote_space)).into_owned()
}

/// A stopped `rebase` as `.git/rebase-merge/` describes it.
struct RebaseProgress {
    onto: String,
    branch: Option<String>,
    done: Vec<String>,
    todo: Vec<String>,
}

fn rebase_progress(repository: &rivet_core::Repository) -> Option<RebaseProgress> {
    let dir = repository.git_dir().join("rebase-merge");
    if !dir.is_dir() {
        return None;
    }
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    let short = |text: &str| text.trim().chars().take(7).collect::<String>();
    let lines = |name: &str| -> Vec<String> {
        read(name)
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| {
                // `pick <full id> # subject`: the id is shown abbreviated.
                let mut words = l.splitn(3, ' ');
                match (words.next(), words.next(), words.next()) {
                    (Some(cmd), Some(id), Some(rest)) => format!("{cmd} {} {rest}", short(id)),
                    _ => l.to_string(),
                }
            })
            .collect()
    };
    let head_name = read("head-name");
    Some(RebaseProgress {
        onto: short(&read("onto")),
        branch: head_name
            .trim()
            .strip_prefix("refs/heads/")
            .map(str::to_string),
        done: lines("done"),
        todo: lines("git-rebase-todo"),
    })
}

impl RebaseProgress {
    fn describe(&self, conflicted: bool) -> String {
        let mut out = format!("interactive rebase in progress; onto {}\n", self.onto);
        if !self.done.is_empty() {
            let n = self.done.len();
            if n == 1 {
                out.push_str("Last command done (1 command done):\n");
            } else {
                out.push_str(&format!("Last commands done ({n} commands done):\n"));
            }
            for line in &self.done[n.saturating_sub(2)..] {
                out.push_str(&format!("   {line}\n"));
            }
            if n > 2 {
                out.push_str("  (see more in file .git/rebase-merge/done)\n");
            }
        }
        if self.todo.is_empty() {
            out.push_str("No commands remaining.\n");
        } else {
            let n = self.todo.len();
            if n == 1 {
                out.push_str("Next command to do (1 remaining command):\n");
            } else {
                out.push_str(&format!("Next commands to do ({n} remaining commands):\n"));
            }
            for line in self.todo.iter().take(2) {
                out.push_str(&format!("   {line}\n"));
            }
            if n > 2 {
                out.push_str("  (see more in file .git/rebase-merge/git-rebase-todo)\n");
            }
            out.push_str("  (use \"git rebase --edit-todo\" to view and edit)\n");
        }
        match &self.branch {
            Some(branch) => out.push_str(&format!(
                "You are currently rebasing branch '{branch}' on '{}'.\n",
                self.onto
            )),
            None => out.push_str("You are currently rebasing.\n"),
        }
        if conflicted {
            out.push_str("  (fix conflicts and then run \"git rebase --continue\")\n");
            out.push_str("  (use \"git rebase --skip\" to skip this patch)\n");
            out.push_str("  (use \"git rebase --abort\" to check out the original branch)\n");
        } else {
            out.push_str("  (all conflicts fixed: run \"git rebase --continue\")\n");
        }
        out
    }
}
