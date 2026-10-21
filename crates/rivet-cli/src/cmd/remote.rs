//! Remote commands: `remote`, `fetch`, `pull` and `push`.
//!
//! Phase 11, milestone **M3**. These are the four commands that talk to another repository, and
//! the only ones that touch the network. Everything above the transport — pkt-line, the
//! capability handshake, want/have negotiation, `report-status` — lives in
//! `rivet_core::remote`; these handlers own the credential lookup, the choice of transport, the
//! progress reporting and the exit codes.
//!
//! *The credential rule for all four: a token comes from `git credential fill` when Git is
//! installed, and from `RIVET_TOKEN` otherwise. It is never written to the config, never echoed
//! in a progress line and never included in an error — every message that names a URL runs it
//! through [`rivet_core::remote::redact_url`] first.*

use anyhow::Result;

use crate::cli::GlobalArgs;

/// Arguments to `rivet remote`.
///
/// # Caveats
///
/// * **`-v` is the global `--verbose` here**, not a subcommand flag: `rivet remote -v` and
///   `rivet -v remote` are the same invocation, and the listing form is selected by the absence
///   of a subcommand rather than by the flag. See [`crate::cli::GlobalArgs`].
/// * **`add` is idempotent in Git only for the same URL, and `--force` is what makes an existing
///   remote accept a new one.** `rivet remote add origin <new-url>` on an existing `origin`
///   fails without `--force`, and the failure is important: silently repointing a remote loses
///   the tracking configuration that `fetch` and `push` depend on.
/// * **A remote's URL is stored unredacted and printed redacted.** The two are not
///   contradictory: `fetch` needs the credentials, and no output may show them. Every path that
///   prints a URL — `-v`, an error, a progress line — uses the redacted form, and the config
///   keeps whatever the user typed.
/// * **`prune` removes remote-tracking refs whose branch is gone from the remote, and it is
///   therefore destructive to local information.** A branch deleted on the remote but merged
///   locally loses its tracking ref; the commits remain reachable from the local branch. It
///   never touches `refs/heads/*`.
/// * **`set-url`, `get-url` and `set-head` write config and nothing else**, so they work in a
///   bare repository and without a work tree — a fact a handler that opened the repository
///   through the work tree would get wrong.
/// * **`rename` moves `branch.<name>.remote` references too.** Renaming `origin` to `upstream`
///   without rewriting them leaves every branch configured to track a remote that no longer
///   exists, which `rivet status` then reports as a missing upstream.
#[derive(Debug, clap::Args)]
pub struct RemoteArgs {
    /// The sub-operation to perform. With none, the remote names are listed.
    #[arg(value_name = "SUBCOMMAND")]
    pub subcommand: Option<String>,

    /// The name of the remote to operate on.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,

    /// The URL for `add` and `set-url`.
    #[arg(value_name = "URL")]
    pub url: Option<String>,

    /// Store the URL as the push URL as well, or as the push URL only.
    #[arg(long = "push")]
    pub push: bool,

    /// Add the URL for fetching only, leaving the push URL alone.
    #[arg(long = "no-push")]
    pub no_push: bool,

    /// Replace an existing remote's URL instead of refusing.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Include or exclude branches that are not fetched into a tracking ref.
    #[arg(short = 't', long = "track", value_name = "BRANCH")]
    pub track: Vec<String>,

    /// Prune remote-tracking refs whose branch has been deleted on the remote.
    #[arg(long = "prune")]
    pub prune: bool,

    /// Fetch every tag from the remote.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Do not fetch tags when `remote.<name>.tagOpt` says to.
    #[arg(long = "no-tags")]
    pub no_tags: bool,

    /// Set the default branch that `remote show` and `clone` report.
    #[arg(long = "auto")]
    pub auto: bool,

    /// Perform the operation for every remote.
    #[arg(long = "all")]
    pub all: bool,

    /// Delete the remote rather than listing or configuring it.
    #[arg(long = "delete")]
    pub delete: bool,
}

/// Arguments to `rivet fetch`.
///
/// # Caveats
///
/// * **`fetch` never touches `refs/heads/*`.** It updates remote-tracking refs
///   (`refs/remotes/<remote>/*`) and, with `--tags`, tag refs. A fetch that wrote to a local
///   branch would move a user's work out from under them, and the mistake is invisible until
///   the next `log`.
/// * **`prune` and `prune_tags` are the only ways a ref is deleted by a fetch**, and both are
///   opt-in. Without them a remote-tracking ref for a branch deleted on the remote lingers for
///   ever, which is the behaviour users most often mistake for a bug.
/// * **`depth` is accepted and not implemented** (plan §12). A shallow fetch must fail loudly:
///   silently fetching full history costs the user a large download and gives them a repository
///   that is deep when they asked for shallow, and the reverse — starting to write a shallow
///   repository without the graft file that makes it usable — is worse.
/// * **`force` is about *ref updates*, not about history.** A remote branch that was rewound
///   produces a non-fast-forward update, which without `--force` is refused with
///   [`rivet_core::Error::NonFastForward`]. With it, the remote-tracking ref moves and the old
///   commits become unreachable from it — recoverable from its reflog, and only until a `gc`.
/// * **The credential is fetched at most once per invocation and cached in memory.** A fetch of
///   a large repository makes several requests, and prompting once per request is the difference
///   between a usable command and one that asks for a password four times.
/// * **`--all` fetches every remote and reports each one's result separately.** A failure on the
///   second remote must not discard the first remote's updated refs, and the exit code must
///   report the failure — the two together are why the loop collects errors rather than
///   short-circuiting with `?`.
#[derive(Debug, clap::Args)]
pub struct FetchArgs {
    /// The remote to fetch from. Defaults to the current branch's upstream, or `origin`.
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,

    /// The refs to fetch, as `refs/heads/*:refs/remotes/origin/*` or a branch name.
    #[arg(value_name = "REFSPEC")]
    pub refspecs: Vec<String>,

    /// Fetch every remote.
    #[arg(long = "all")]
    pub all: bool,

    /// Fetch every configured remote and prune each one.
    #[arg(long = "multiple")]
    pub multiple: bool,

    /// Delete remote-tracking refs whose branch is gone from the remote.
    #[arg(short = 'p', long = "prune")]
    pub prune: bool,

    /// Fetch every tag, or only the ones that point into the fetched history.
    #[arg(short = 't', long = "tags")]
    pub tags: bool,

    /// Do not fetch tags.
    #[arg(short = 'n', long = "no-tags")]
    pub no_tags: bool,

    /// Limit the history to this many commits. Not implemented in v1 (plan §12).
    #[arg(long = "depth", value_name = "N")]
    pub depth: Option<u32>,

    /// Extend a shallow fetch to a deeper one. Not implemented in v1.
    #[arg(long = "deepen", value_name = "N")]
    pub deepen: Option<u32>,

    /// Unshallow the repository. Not implemented in v1.
    #[arg(long = "unshallow")]
    pub unshallow: bool,

    /// Allow a non-fast-forward update to a remote-tracking ref.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Write the fetched refs without checking whether the objects arrived.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Update `FETCH_HEAD` without writing any tracking refs.
    #[arg(long = "no-write-fetch-head")]
    pub no_write_fetch_head: bool,

    /// Enable or disable negotiation progress output.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Fetch the objects into a pack but do not index them. Not implemented in v1.
    #[arg(long = "keep")]
    pub keep: bool,

    /// Print the refs that would be updated without fetching anything.
    #[arg(long = "prune-tags")]
    pub prune_tags: bool,

    /// Write every fetched ref under `FETCH_HEAD` rather than a tracking ref.
    #[arg(long = "atomic")]
    pub atomic: bool,

    /// Refuse to fetch when the remote's object format differs from the local one.
    #[arg(long = "set-upstream")]
    pub set_upstream: bool,
}

/// Arguments to `rivet pull`.
///
/// # Caveats
///
/// * **A pull is a fetch followed by a merge, and `rebase` makes it a fetch followed by a
///   rebase.** `--rebase` and `--no-rebase` are opposites and the default comes from
///   `branch.<name>.rebase`, which is why neither can be the hard-coded behaviour: a user who
///   set that config and gets a merge commit has been ignored.
/// * **A conflicted pull leaves the repository in the middle of a *merge***, with `MERGE_HEAD`
///   written and stages in the index — not in some pull-specific state, and not having fetched
///   and then failed. The fetch has already succeeded and its ref updates are kept; that is what
///   makes `rivet pull` after resolving the conflicts continue where it left off rather than
///   re-downloading.
/// * **`--ff-only` refuses rather than merging**, and its refusal is exit `128` with the local
///   branch untouched. It is the only form of pull that can never create a commit, which is why
///   a script that must not invent history uses it.
/// * **`--autostash` is not implemented** and must say so. A pull that silently left a dirty
///   work tree unstashed fails at the merge step with an unrelated-looking error, and a pull
///   that stashed without being asked loses the user's context.
/// * **Pulling into a repository with no upstream is an error**, not an implicit `origin`. A
///   branch with no `branch.<name>.merge` has no defined source, and guessing `origin/<name>`
///   produces a merge from a branch the user did not name.
#[derive(Debug, clap::Args)]
pub struct PullArgs {
    /// The remote to pull from. Defaults to the current branch's upstream.
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,

    /// The refs to fetch, as a refspec.
    #[arg(value_name = "REFSPEC")]
    pub refspecs: Vec<String>,

    /// Rebase the local commits onto the fetched branch instead of merging.
    #[arg(short = 'r', long = "rebase")]
    pub rebase: bool,

    /// Merge the fetched branch, overriding `branch.<name>.rebase`. The default.
    #[arg(long = "no-rebase")]
    pub no_rebase: bool,

    /// Refuse to pull when the result would not be a fast-forward.
    #[arg(long = "ff-only")]
    pub ff_only: bool,

    /// Create a merge commit even when a fast-forward is possible.
    #[arg(long = "no-ff")]
    pub no_ff: bool,

    /// Stage the merge result and leave `HEAD` alone, for a single later commit.
    #[arg(long = "squash")]
    pub squash: bool,

    /// Fetch every remote and merge the current branch's upstream.
    #[arg(long = "all")]
    pub all: bool,

    /// Delete remote-tracking refs whose branch is gone, before merging.
    #[arg(short = 'p', long = "prune")]
    pub prune: bool,

    /// Fetch every tag.
    #[arg(short = 't', long = "tags")]
    pub tags: bool,

    /// Do not fetch tags.
    #[arg(short = 'n', long = "no-tags")]
    pub no_tags: bool,

    /// Stash local changes before pulling and restore them after. Not implemented in v1.
    #[arg(long = "autostash")]
    pub autostash: bool,

    /// The commit to merge, as an alternative to `--rebase`.
    #[arg(long = "no-autostash")]
    pub no_autostash: bool,

    /// Allow a pull between two histories that share no ancestor.
    #[arg(long = "allow-unrelated-histories")]
    pub allow_unrelated_histories: bool,

    /// Rebase the local commits interactively. Not implemented in v1.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// The signature of the merge commit, for a signed pull. Not implemented in v1.
    #[arg(short = 'S', long = "gpg-sign", value_name = "KEY")]
    pub gpg_sign: Option<Option<String>>,

    /// Do not run the pre-merge and post-merge hooks. They are not implemented in v1.
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Show what would be fetched and merged, and change nothing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Report progress on stderr.
    #[arg(long = "progress")]
    pub progress: bool,
}

/// Arguments to `rivet push`.
///
/// # Caveats
///
/// * **A push is refused unless it fast-forwards, and `--force` is the only thing that changes
///   that.** The refusal is [`rivet_core::Error::NonFastForward`] and it must name the remote
///   ref, the old id and the new one, because the user's next action is to fetch and look at
///   what they are about to discard. `--force-with-lease` is the safe form and is the one a
///   script should use.
/// * **`--delete` and `--tags` are different operations that share the ref-update code.** A
///   delete sends `(old, zero)`; a tag push sends `(zero, new)` for a ref that did not exist.
///   The zero id is what tells the remote which it is, and it is format-dependent — see
///   [`rivet_core::ObjectId::zero`] — so it must not be spelled with 40 literal zeroes.
/// * **An empty `refspec` with `push.default` unset is an error, not a guess.** Git has changed
///   what `git push` with no arguments means several times; Rivet reads `push.default` and fails
///   with the setting's name when it is `nothing` or absent, rather than picking a default that
///   may not be the user's.
/// * **`--delete` on a ref that the remote does not have succeeds.** The remote reports it as
///   already-deleted and `report-status` says so; treating it as a failure makes a cleanup
///   script non-idempotent.
/// * **A rejected ref does not fail the whole push.** `report-status` reports per ref, and one
///   rejected ref in a multi-ref push still updates the others — so the exit code is `1` when
///   *any* ref was rejected, while the accepted ones are already written on the remote. A
///   handler that rolled back or that reported total failure would misdescribe what happened.
/// * **`--mirror` deletes remote refs to match the local ones**, which is the most destructive
///   thing this command can do. It is not the same as `--all`, which pushes every branch without
///   deleting anything, and the two must not share an implementation.
/// * **`--force-with-lease` with no expected id uses the remote-tracking ref's current value**,
///   which is the whole point: it is a force push that still refuses if somebody else has pushed
///   since the last fetch. Sending a bare force instead defeats it silently.
#[derive(Debug, clap::Args)]
pub struct PushArgs {
    /// The remote to push to. Defaults to the current branch's upstream, or `origin`.
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,

    /// The refs to push, as `src:dst`, a branch name, or `:` to delete.
    #[arg(value_name = "REFSPEC")]
    pub refspecs: Vec<String>,

    /// Push every branch, and the tags that point into them.
    #[arg(long = "all")]
    pub all: bool,

    /// Push the tags as well as the refs named.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Push every ref and delete the remote's refs that are not local.
    #[arg(long = "mirror")]
    pub mirror: bool,

    /// Delete the named refs on the remote.
    #[arg(long = "delete")]
    pub delete: bool,

    /// Force the update even when it is not a fast-forward.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Force the update only if the remote ref still has the value the last fetch saw.
    #[arg(long = "force-with-lease", value_name = "REF")]
    pub force_with_lease: Option<Option<String>>,

    /// Use this value as the expected remote ref for `--force-with-lease`.
    #[arg(long = "force-if-includes")]
    pub force_if_includes: bool,

    /// Do not push at all; report what would be pushed.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Set the upstream of each pushed branch.
    #[arg(short = 'u', long = "set-upstream")]
    pub set_upstream: bool,

    /// Push every branch that has no upstream yet, as `push.default=current` would.
    #[arg(long = "porcelain")]
    pub porcelain: bool,

    /// Update the remote-tracking refs without pushing anything. Not implemented in v1.
    #[arg(long = "thin", value_name = "MODE")]
    pub thin: Option<Option<String>>,

    /// Send only the objects the remote does not have, relative to this ref.
    #[arg(long = "atomic")]
    pub atomic: bool,

    /// Report progress on stderr.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Skip the pre-push hook. It is not implemented in v1 (plan §1).
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// The refs to receive on the remote, in `--receive-pack` form.
    #[arg(long = "receive-pack", value_name = "PATH")]
    pub receive_pack: Option<String>,

    /// The program to run on the remote, for the SSH transport.
    #[arg(long = "exec", value_name = "PATH")]
    pub exec: Option<String>,

    /// Follow tags that point into the pushed history. Not implemented in v1.
    #[arg(long = "follow-tags")]
    pub follow_tags: bool,

    /// Push the tag objects themselves even when they are not reachable.
    #[arg(long = "signed", value_name = "MODE")]
    pub signed: Option<Option<String>>,
}

/// Phase 11 — manage the set of remotes.
///
/// # Purpose
///
/// Add, remove, rename and configure remotes: the `[remote "<name>"]` sections that every other
/// network command reads. Lists the remotes when given no sub-operation.
///
/// # Exit codes
///
/// * `0` — the operation was carried out, or the listing was produced.
/// * `2` — an unknown sub-operation was given. Git returns `2` here rather than `128` because it
///   is a usage error, and scripts distinguish the two.
/// * `128` — the remote does not exist, it already exists without `--force`, the URL is
///   unparseable, or no repository was found for a sub-operation that needs one.
///
/// # Caveats
///
/// * **A remote's name is a config subsection, so it is case-sensitive and may contain almost
///   anything.** `remote.Origin.url` and `remote.origin.url` are different remotes; a handler
///   that lowercased the name would merge them. The name may not contain a newline or a `]`,
///   which is what makes the INI subsection unambiguous.
/// * **`add` writes two keys, not one**: `remote.<name>.url` and, unless `--no-push` was given,
///   `remote.<name>.pushurl`. It does not write a `fetch` refspec — that comes from `clone` or
///   from `--track` — so a remote added by hand and then fetched uses the default refspec, which
///   is why a handler must not assume `remote.<name>.fetch` exists.
/// * **Removing a remote does not remove its remote-tracking refs.** `git remote remove` deletes
///   the config section and the tracking refs; `git remote rm` in older versions deleted only
///   the config. Rivet follows the current behaviour and says what it deleted, because a user
///   who expected the refs to survive has lost their only local copy of a deleted branch's
///   commits.
/// * **`show` contacts the remote and `-v` does not.** The two are different commands with the
///   same name and a handler must not make the offline one perform I/O — `rivet remote -v` in a
///   script with no network must succeed.
/// * **`set-head` writes `refs/remotes/<name>/HEAD` as a symbolic ref**, not as a direct one, so
///   that it follows the remote branch it names. Writing a direct ref makes it stale the moment
///   the remote's `HEAD` moves.
/// * **`--push` and `--no-push` are not opposites of `add` and `set-url` in the same way**:
///   `--push` sets the *push* URL only, while the default sets both. The three states — fetch
///   URL only, push URL only, both — cannot be expressed with one `Option<String>` and the
///   grammar above spells them as two booleans for that reason.
pub fn run_remote(global: &GlobalArgs, args: &RemoteArgs) -> Result<i32> {
    let mut repository = super::open(global)?;
    let verbose = global.verbose && !global.quiet;
    let Some(subcommand) = args.subcommand.as_deref() else {
        let mut names = repository.remote_names();
        names.sort();
        for name in names {
            if verbose {
                let config = repository.remote_config(&name.to_string())?.expect("listed remote exists");
                let fetch = config.urls.first().map(|u| u.to_string()).unwrap_or_default();
                let push = config.push_urls.first().map(|u| u.to_string()).unwrap_or_else(|| fetch.clone());
                println!("{name}\t{fetch} (fetch)");
                println!("{name}\t{push} (push)");
            } else {
                println!("{name}");
            }
        }
        return Ok(0);
    };
    let exists = |repository: &Repository, name: &str| repository.remote_config(name).ok().flatten().is_some();
    match subcommand {
        "add" => {
            let (Some(name), Some(url)) = (&args.name, &args.url) else {
                eprintln!("usage: git remote add [<options>] <name> <url>");
                return Ok(129);
            };
            if name.is_empty() || name.contains(' ') || name.contains('/') && false {
                anyhow::bail!("'{name}' is not a valid remote name");
            }
            if exists(&repository, name) {
                eprintln!("error: remote {name} already exists.");
                return Ok(3);
            }
            let config = repository.config_mut();
            config.set(BStr::new(format!("remote.{name}.url").as_bytes()), url.as_bytes())?;
            if args.track.is_empty() {
                config.set(
                    BStr::new(format!("remote.{name}.fetch").as_bytes()),
                    format!("+refs/heads/*:refs/remotes/{name}/*").as_bytes(),
                )?;
            } else {
                for branch in &args.track {
                    config.add(
                        BStr::new(format!("remote.{name}.fetch").as_bytes()),
                        format!("+refs/heads/{branch}:refs/remotes/{name}/{branch}").as_bytes(),
                    )?;
                }
            }
            if args.tags {
                config.set(BStr::new(format!("remote.{name}.tagopt").as_bytes()), b"--tags")?;
            } else if args.no_tags {
                config.set(BStr::new(format!("remote.{name}.tagopt").as_bytes()), b"--no-tags")?;
            }
            repository.write_config()?;
            if args.fetch_after_add() {
                let request = FetchRequest { write_fetch_head: true, ..Default::default() };
                let report = repository.fetch_remote(name, &request)?;
                print_fetch_report(&report, global.quiet);
            }
            Ok(0)
        }
        "remove" | "rm" => {
            let Some(name) = &args.name else {
                eprintln!("usage: git remote remove <name>");
                return Ok(129);
            };
            if !exists(&repository, name) {
                eprintln!("error: No such remote: '{name}'");
                return Ok(2);
            }
            // Its remote-tracking refs go with it, as do the branch settings that named it.
            let prefix = format!("refs/remotes/{name}/");
            for (reference, _) in repository.refs().list(prefix.as_bytes())? {
                repository.refs().delete(&reference)?;
            }
            let branches: Vec<String> = repository
                .config()
                .entries()
                .iter()
                .filter(|e| {
                    e.section.eq_ignore_ascii_case(b"branch")
                        && e.key.eq_ignore_ascii_case(b"remote")
                        && e.value.as_slice() == name.as_bytes()
                })
                .filter_map(|e| e.subsection.as_ref().map(|s| s.to_string()))
                .collect();
            let config = repository.config_mut();
            config.remove_section(BStr::new("remote"), Some(BStr::new(name.as_bytes())));
            for branch in branches {
                let _ = config.remove(BStr::new(format!("branch.{branch}.remote").as_bytes()));
                let _ = config.remove(BStr::new(format!("branch.{branch}.merge").as_bytes()));
            }
            repository.write_config()?;
            Ok(0)
        }
        "rename" => {
            let (Some(old), Some(new)) = (&args.name, &args.url) else {
                eprintln!("usage: git remote rename <old> <new>");
                return Ok(129);
            };
            if !exists(&repository, old) {
                eprintln!("error: No such remote: '{old}'");
                return Ok(2);
            }
            if exists(&repository, new) {
                eprintln!("error: remote {new} already exists.");
                return Ok(3);
            }
            let moved: Vec<(String, String)> = repository
                .config()
                .entries()
                .iter()
                .filter(|e| e.section.eq_ignore_ascii_case(b"remote") && e.subsection.as_ref().is_some_and(|s| s.as_slice() == old.as_bytes()))
                .map(|e| (e.key.to_string(), e.value.to_string()))
                .collect();
            let branches: Vec<String> = repository
                .config()
                .entries()
                .iter()
                .filter(|e| {
                    e.section.eq_ignore_ascii_case(b"branch")
                        && e.key.eq_ignore_ascii_case(b"remote")
                        && e.value.as_slice() == old.as_bytes()
                })
                .filter_map(|e| e.subsection.as_ref().map(|s| s.to_string()))
                .collect();
            let config = repository.config_mut();
            config.remove_section(BStr::new("remote"), Some(BStr::new(old.as_bytes())));
            for (key, value) in moved {
                let value = value.replace(&format!("refs/remotes/{old}/"), &format!("refs/remotes/{new}/"));
                config.add(BStr::new(format!("remote.{new}.{key}").as_bytes()), value.as_bytes())?;
            }
            for branch in branches {
                config.set(BStr::new(format!("branch.{branch}.remote").as_bytes()), new.as_bytes())?;
            }
            repository.write_config()?;
            // Move the tracking refs and their reflogs.
            let from = format!("refs/remotes/{old}/");
            for (reference, _) in repository.refs().list(from.as_bytes())? {
                let tail = &reference.as_bytes()[from.len()..];
                let target = RefName::from_bytes(format!("refs/remotes/{new}/{}", String::from_utf8_lossy(tail)).as_bytes())?;
                if reference.as_bytes().ends_with(b"/HEAD") {
                    // The symbolic HEAD must follow, pointing into the new namespace.
                    if let Some(rivet_core::RefTarget::Symbolic(to)) = repository.refs().try_read(&reference)? {
                        let new_to = RefName::from_bytes(
                            String::from_utf8_lossy(to.as_bytes()).replace(&from, &format!("refs/remotes/{new}/")).as_bytes(),
                        )?;
                        repository.refs().set_symbolic(&target, &new_to, None)?;
                        repository.refs().delete(&reference)?;
                    }
                } else {
                    repository.refs().rename(&reference, &target)?;
                }
            }
            Ok(0)
        }
        "get-url" => {
            let Some(name) = &args.name else {
                eprintln!("usage: git remote get-url [--push] [--all] <name>");
                return Ok(129);
            };
            let Some(config) = repository.remote_config(name)? else {
                eprintln!("error: No such remote '{name}'");
                return Ok(2);
            };
            let url = if args.push { config.push_urls.first().or(config.urls.first()) } else { config.urls.first() };
            println!("{}", url.map(|u| u.to_string()).unwrap_or_default());
            Ok(0)
        }
        "set-url" => {
            let (Some(name), Some(url)) = (&args.name, &args.url) else {
                eprintln!("usage: git remote set-url [--push] <name> <newurl> [<oldurl>]");
                return Ok(129);
            };
            if !exists(&repository, name) {
                eprintln!("error: No such remote '{name}'");
                return Ok(2);
            }
            let key = if args.push { "pushurl" } else { "url" };
            repository.config_mut().set(BStr::new(format!("remote.{name}.{key}").as_bytes()), url.as_bytes())?;
            repository.write_config()?;
            Ok(0)
        }
        other => anyhow::bail!("remote: subcommand '{other}' is not implemented"),
    }
}

/// Phase 11 — download objects and refs from another repository.
///
/// # Purpose
///
/// Connect to a remote, learn what it has, negotiate the objects the local repository is
/// missing, receive and index the pack, and update the remote-tracking refs — leaving
/// `refs/heads/*` untouched.
///
/// # Exit codes
///
/// * `0` — the fetch completed. "Already up to date" is a success that prints one line.
/// * `1` — the remote's URL is unreachable or the remote does not exist *and* `--all` was given,
///   so that some remotes succeeded. A partial failure.
/// * `128` — the remote does not exist, the URL is unparseable, authentication failed, the
///   object formats differ, a ref update would be non-fast-forward without `--force`, or the
///   pack could not be indexed.
///
/// # Caveats
///
/// * **`refs/heads/*` is never written.** The one exception is a refspec that explicitly names a
///   local branch as its destination — `+refs/heads/*:refs/heads/*` — which is how
///   `remote.<name>.mirror` works, and which must be honoured when given and never inferred.
/// * **The pack may be thin, and resolving a thin pack requires objects the local repository
///   already has.** Receiving one and failing to fix it up produces a `.pack` that `git fsck`
///   reports as broken; the fix-up is `rivet_core::pack::fix_thin_pack` and it must run before
///   the pack is written into `objects/pack/`, not after.
/// * **Ref updates happen after the objects are durable.** A fetch that moved a tracking ref and
///   then failed to write the pack leaves a ref pointing at an object that is not there, which
///   is the one state that makes a repository unrepairable without re-fetching.
/// * **A tag that points at a commit outside the fetched history still needs its object**, which
///   is what `include-tag` negotiates. A fetch of a branch whose tags point into history the
///   branch does not contain must either fetch those objects or decline the tag, and adding the
///   ref without the object is not an option.
/// * **The negotiation is bounded.** [`rivet_core::MAX_NEGOTIATION_ROUNDS`] rounds of
///   `have`/`ACK` is the cap, and a remote that never says `ready` must fall back to sending
///   more haves rather than looping; a fetch of a repository with a large divergence is where
///   this is exercised.
/// * **`FETCH_HEAD` is written last and in Git's format**, including the `not-for-merge` marker
///   on every line but the one the current branch tracks. `pull` reads it, and a `FETCH_HEAD`
///   missing the marker makes `pull` merge the wrong ref.
/// * **A fetch into a bare repository is normal and needs no work tree**, so the handler must
///   not call `require_worktree` for any part of it except the `pull` that may follow.
pub fn run_fetch(global: &GlobalArgs, args: &FetchArgs) -> Result<i32> {
    reject_unsupported_fetch(args.depth.is_some() || args.deepen.is_some() || args.unshallow, "--depth/--deepen/--unshallow")?;
    if args.atomic || args.keep || args.set_upstream || args.multiple {
        anyhow::bail!("fetch: --atomic, --keep, --set-upstream and --multiple are not implemented");
    }
    let mut repository = super::open(global)?;
    let remotes: Vec<String> = if args.all {
        repository.remote_names().iter().map(|n| n.to_string()).collect()
    } else {
        vec![default_remote(&repository, args.remote.as_deref())?]
    };
    let mut refspecs = Vec::new();
    for spec in &args.refspecs {
        refspecs.push(rivet_core::remote_ops::Refspec::parse(BStr::new(spec.as_bytes()))?);
    }
    let mut code = 0;
    for remote in remotes {
        let request = FetchRequest {
            refspecs: refspecs.clone(),
            tags: if args.tags { TagMode::All } else if args.no_tags { TagMode::None } else { TagMode::Auto },
            prune: args.prune,
            force: args.force,
            dry_run: args.dry_run,
            write_fetch_head: !args.no_write_fetch_head,
            pack_refs: false,
            action: command_action("fetch"),
        };
        let report = repository.fetch_remote(&remote, &request)?;
        print_fetch_report(&report, global.quiet);
        if report.rejected > 0 {
            eprintln!("error: some local refs could not be updated; try running\n 'git remote prune {remote}' to remove any old, conflicting branches");
            code = 1;
        }
    }
    Ok(code)
}

/// Phase 11 — fetch from a remote and merge into the current branch.
///
/// # Purpose
///
/// The two-step command every Git user types most: fetch the upstream of the current branch, then
/// fast-forward the local branch onto it — or merge, or rebase, as the options and the
/// `branch.<name>.rebase` setting dictate.
///
/// # Exit codes
///
/// * `0` — the fetch and the merge both succeeded.
/// * `1` — the merge left conflicts, and the merge is in progress. The same state and the same
///   code as `rivet merge`.
/// * `128` — no upstream is configured, the fetch failed, `--ff-only` and the histories have
///   diverged, or the work tree is dirty and `--autostash` was not given.
///
/// # Caveats
///
/// * **The merge half is `rivet merge`, not a second implementation.** It must go through the
///   same pre-merge safety check, write the same `MERGE_HEAD`/`MERGE_MSG` and the same index
///   stages, so that `rivet merge --abort` works on a conflicted pull. A pull with its own merge
///   path is a repository where conflict resolution behaves differently depending on which
///   command produced the conflict.
/// * **`--ff-only` is a promise that no commit is created**, and it must be checked against the
///   merge base *before* the work tree is touched. Refusing halfway leaves a work tree that
///   matches neither branch.
/// * **The local commits are not rewritten by a merge**, and `--rebase` is the flag that changes
///   that. A pull that rebased without being asked has rewritten the user's history, and a pull
///   that merged when `branch.<name>.rebase = true` has added a commit they configured away.
/// * **A pull with a dirty work tree refuses**, and the refusal names the paths. Auto-stashing
///   is not implemented, so the message must say what to do — commit or stash — rather than
///   referring to a flag that does nothing.
/// * **`--all` fetches every remote but merges only the current branch's upstream**, which is a
///   combination whose name suggests more than it does; the progress output must distinguish the
///   remotes that were fetched from the one that was merged.
/// * **A tag fetched by the pull is not merged.** `--tags` fetches tags; it does not make the
///   current branch point at one, and a handler that found "the newest tag" and merged it would
///   be inventing an upstream.
pub fn run_pull(global: &GlobalArgs, args: &PullArgs) -> Result<i32> {
    if args.rebase || args.autostash || args.interactive || args.squash || args.gpg_sign.is_some() {
        anyhow::bail!("pull: --rebase, --autostash, --interactive, --squash and --gpg-sign are not implemented");
    }
    let mut repository = super::open(global)?;
    let head = repository.head()?;
    let branch = head.branch().map(|b| b.short_name().to_string());
    let upstream = match &branch {
        Some(b) => repository.branch_upstream(b)?,
        None => None,
    };
    let remote = match (&args.remote, &upstream) {
        (Some(remote), _) => remote.clone(),
        (None, Some((remote, _))) => remote.clone(),
        (None, None) => {
            if branch.is_none() {
                eprintln!("You are not currently on a branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
            } else {
                let b = branch.as_deref().unwrap_or_default();
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!("If you wish to set tracking information for this branch you can do so with:");
                eprintln!();
                eprintln!("    git branch --set-upstream-to=origin/<branch> {b}");
                eprintln!();
            }
            return Ok(1);
        }
    };
    let mut refspecs = Vec::new();
    for spec in &args.refspecs {
        refspecs.push(rivet_core::remote_ops::Refspec::parse(BStr::new(spec.as_bytes()))?);
    }
    let request = FetchRequest {
        refspecs,
        tags: if args.tags { TagMode::All } else if args.no_tags { TagMode::None } else { TagMode::Auto },
        prune: args.prune,
        force: false,
        dry_run: args.dry_run,
        write_fetch_head: true,
        pack_refs: false,
        action: command_action("pull"),
    };
    let report = repository.fetch_remote(&remote, &request)?;
    print_fetch_report(&report, global.quiet);
    if args.dry_run {
        return Ok(0);
    }

    // The branch to merge: the ones fetch flagged, which are the upstream branch or the ones
    // named on the command line.
    let merge_heads: Vec<(rivet_core::ObjectId, String)> = report
        .fetch_head
        .iter()
        .filter(|(_, for_merge, _)| *for_merge)
        .map(|(oid, _, description)| (*oid, description.clone()))
        .collect();
    let Some((theirs, description)) = merge_heads.first().cloned() else {
        eprintln!("You asked to pull from the remote '{remote}', but did not specify");
        eprintln!("a branch. Because this is not the default configured remote");
        eprintln!("for your current branch, you must specify a branch on the command line.");
        return Ok(1);
    };
    if merge_heads.len() > 1 {
        anyhow::bail!("pull: merging more than one branch is not implemented");
    }
    let ff = if args.ff_only {
        super::branching::FastForward::Only
    } else if args.no_ff {
        super::branching::FastForward::Never
    } else {
        super::branching::FastForward::Allow
    };
    let message = format!("Merge {description}");
    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(&repository, "merge.renames");
    super::branching::merge_commit(
        &mut repository,
        theirs,
        &description,
        &message,
        ff,
        false,
        args.allow_unrelated_histories,
        &options,
        !global.quiet,
        &command_action("pull"),
        false,
    )
}

/// Phase 11 — update remote refs along with the objects they need.
///
/// # Purpose
///
/// Compute the ref updates to send, pack every object reachable from them that the remote does
/// not advertise, send the pack and the update commands, and parse `report-status` into a
/// per-ref result.
///
/// # Exit codes
///
/// * `0` — every ref update was accepted.
/// * `1` — at least one ref update was rejected by the remote. **The others were applied**; the
///   remote is in a mixed state and the per-ref status says which.
/// * `128` — no remote is configured, the URL is unparseable or unreachable, authentication
///   failed, the object formats differ, the local fast-forward check failed without `--force`,
///   or the pack could not be written.
///
/// # Caveats
///
/// * **Nothing is sent until the whole pack is built.** A push that streamed objects and then
///   discovered a non-fast-forward has already put objects on the remote that its refs do not
///   reach — unreferenced but present — and cannot take them back. The update commands are
///   computed and validated first, and the local fast-forward check runs before anything leaves
///   the machine.
/// * **`report-status` is per ref and the exit code is the aggregate**, per [`PushArgs`]. A push
///   of three branches that succeeds on two exits `1` and prints both the rejection and the two
///   successes, because a user who reads only the exit code must not conclude nothing happened.
/// * **The `unpack` line is not a ref result.** A remote whose unpack failed rejects every ref
///   with the same message, and reporting it once per ref multiplies one failure into twenty
///   lines; it is the `unpack_ok` flag on [`rivet_core::PushOutcome`] that carries it.
/// * **A pushed tag must have its objects in the pack.** Pushing `refs/tags/v1` sends the tag
///   object and everything it peels to; a handler that used the same reachability walk as for a
///   branch and stopped at the tag's target would send history the remote already has and omit
///   the tag object itself, and the push is rejected with "missing necessary objects".
/// * **`--force-with-lease` compares against the *remote-tracking ref*, not against a fresh
///   read of the remote.** That is deliberate — it is what makes it detect a concurrent push
///   since the last fetch — and it means a stale tracking ref causes a spurious rejection. The
///   message must say to fetch, because that is the fix.
/// * **Credentials are never logged, even at maximum verbosity.** The HTTP transport sends an
///   `Authorization` header and the SSH transport spawns a process with the URL in its argv; in
///   both cases the value passed to a log line is the redacted URL, and no diagnostic prints the
///   header or the argv verbatim. See [`rivet_core::remote::redact_url`] and
///   `rivet_core::remote::GIT_SSH_COMMAND_ENV`.
/// * **An empty push is a success**, and says "Everything up-to-date". It is not an error and
///   must not be `128`, or a script that pushes on every build reports a failure whenever
///   nothing changed.
pub fn run_push(global: &GlobalArgs, args: &PushArgs) -> Result<i32> {
    if args.mirror || args.follow_tags || args.signed.is_some() || args.atomic || args.force_with_lease.is_some()
        || args.force_if_includes || args.porcelain || args.receive_pack.is_some() || args.exec.is_some()
    {
        anyhow::bail!("push: --mirror, --follow-tags, --signed, --atomic, --force-with-lease, --force-if-includes, --porcelain and --receive-pack are not implemented");
    }
    let mut repository = super::open(global)?;
    let head = repository.head()?;
    let branch = head.branch().map(|b| b.short_name().to_string());
    let upstream = match &branch {
        Some(b) => repository.branch_upstream(b)?,
        None => None,
    };
    let remote = match (&args.remote, &upstream) {
        (Some(remote), _) => remote.clone(),
        (None, Some((remote, _))) if remote != "." => remote.clone(),
        _ => "origin".to_string(),
    };
    if repository.remote_config(&remote)?.is_none() && !remote.contains('/') && !remote.contains(':') && !remote.contains('.') {
        eprintln!("fatal: '{remote}' does not appear to be a git repository");
        eprintln!("fatal: Could not read from remote repository.");
        eprintln!();
        eprintln!("Please make sure you have the correct access rights");
        eprintln!("and the repository exists.");
        return Ok(128);
    }

    let mut specs = Vec::new();
    for spec in &args.refspecs {
        let mut parsed = rivet_core::remote_ops::Refspec::parse(BStr::new(spec.as_bytes()))?;
        if args.delete {
            parsed.dst = parsed.src.clone();
            parsed.src = bstr::BString::default();
        }
        specs.push(parsed);
    }
    let mut set_upstream_to: Option<(String, String)> = None;
    if specs.is_empty() && !args.all && !args.tags {
        // push.default = simple: the current branch, to its upstream of the same name.
        let Some(branch) = branch.clone() else {
            anyhow::bail!("You are not currently on a branch.\nTo push the history leading to the current (detached HEAD)\nstate now, use\n\n    git push {remote} HEAD:<name-of-remote-branch>");
        };
        match &upstream {
            Some((_, merge)) if merge.strip_prefix("refs/heads/") == Some(branch.as_str()) => {
                specs.push(rivet_core::remote_ops::Refspec::parse(BStr::new(branch.as_bytes()))?);
            }
            Some((_, merge)) => {
                eprintln!("fatal: The upstream branch of your current branch does not match");
                eprintln!("the name of your current branch.  To push to the upstream branch");
                eprintln!("on the remote, use");
                eprintln!();
                eprintln!("    git push {remote} HEAD:{merge}");
                eprintln!();
                eprintln!("To push to the branch of the same name on the remote, use");
                eprintln!();
                eprintln!("    git push {remote} HEAD");
                return Ok(128);
            }
            None if args.set_upstream => {
                specs.push(rivet_core::remote_ops::Refspec::parse(BStr::new(branch.as_bytes()))?);
            }
            None => {
                eprintln!("fatal: The current branch {branch} has no upstream branch.");
                eprintln!("To push the current branch and set the remote as upstream, use");
                eprintln!();
                eprintln!("    git push --set-upstream {remote} {branch}");
                eprintln!();
                eprintln!("To have this happen automatically for branches without a tracking");
                eprintln!("upstream, see 'push.autoSetupRemote' in 'git help config'.");
                eprintln!();
                return Ok(128);
            }
        }
    }
    if args.set_upstream {
        if let Some(spec) = specs.first() {
            let local = if spec.src.is_empty() { branch.clone().unwrap_or_default() } else { spec.src.to_string() };
            let local = if local == "HEAD" { branch.clone().unwrap_or_default() } else { local };
            let dst = if spec.dst.is_empty() { local.clone() } else { spec.dst.to_string() };
            set_upstream_to = Some((local, dst));
        }
    }

    let request = PushRequest {
        refspecs: specs,
        all: args.all,
        tags: args.tags,
        force: args.force,
        delete: args.delete,
        dry_run: args.dry_run,
    };
    let report = repository.push_remote(&remote, &request)?;

    let everything_current = report.changes.iter().all(|c| c.flag == '=');
    if !global.quiet {
        if everything_current {
            eprintln!("Everything up-to-date");
        } else {
            eprintln!("To {}", display_url(&report.url, false));
            for change in &report.changes {
                eprintln!("{}", change.push_line());
            }
        }
    }
    if report.rejected > 0 {
        eprintln!("error: failed to push some refs to '{}'", report.url);
        if report.fetch_first {
            eprintln!("hint: Updates were rejected because the remote contains work that you do not");
            eprintln!("hint: have locally. This is usually caused by another repository pushing to");
            eprintln!("hint: the same ref. If you want to integrate the remote changes, use");
            eprintln!("hint: 'git pull' before pushing again.");
            eprintln!("hint: See the 'Note about fast-forwards' in 'git push --help' for details.");
        } else if report.non_fast_forward {
            let current = report.changes.iter().any(|c| c.flag == '!' && Some(c.from.as_str()) == branch.as_deref() && c.from == c.to);
            if current {
                eprintln!("hint: Updates were rejected because the tip of your current branch is behind");
                eprintln!("hint: its remote counterpart. If you want to integrate the remote changes,");
                eprintln!("hint: use 'git pull' before pushing again.");
            } else {
                eprintln!("hint: Updates were rejected because a pushed branch tip is behind its remote");
                eprintln!("hint: counterpart. If you want to integrate the remote changes, use 'git pull'");
                eprintln!("hint: before pushing again.");
            }
            eprintln!("hint: See the 'Note about fast-forwards' in 'git push --help' for details.");
        }
        return Ok(1);
    }
    if let (Some((local, dst)), false) = (set_upstream_to, args.dry_run) {
        let local_ref = RefName::branch(BStr::new(local.as_bytes()))?;
        let tracking = RefName::from_bytes(format!("refs/remotes/{remote}/{dst}").as_bytes())?;
        if repository.refs().exists(&local_ref) && repository.refs().exists(&tracking) {
            let message = super::branching::set_upstream(&mut repository, &local_ref, &tracking)?;
            println!("{message}");
        }
    }
    Ok(0)
}

use bstr::BStr;
use rivet_core::remote_ops::{FetchRequest, PushRequest, TagMode};
use rivet_core::{RefName, Repository};

impl RemoteArgs {
    /// `remote add -f` is not in the grammar; the flag set has `--force`, which Git spells `-f`.
    fn fetch_after_add(&self) -> bool {
        self.force
    }
}

fn reject_unsupported_fetch(condition: bool, what: &str) -> Result<()> {
    if condition {
        anyhow::bail!("fetch: {what} is not implemented");
    }
    Ok(())
}

/// The remote a fetch uses when none is named: the current branch's, else `origin`.
fn default_remote(repository: &Repository, named: Option<&str>) -> Result<String> {
    if let Some(name) = named {
        return Ok(name.to_string());
    }
    if let Some(branch) = repository.head()?.branch() {
        if let Some((remote, _)) = repository.branch_upstream(&branch.short_name().to_string())? {
            if remote != "." {
                return Ok(remote);
            }
        }
    }
    Ok("origin".to_string())
}

/// Git shows a remote URL without a trailing slash or `.git` in `From <url>`.
pub fn display_url(url: &str, strip_git: bool) -> String {
    let mut shown = url.trim_end_matches('/').to_string();
    if strip_git {
        if let Some(stripped) = shown.strip_suffix(".git") {
            shown = stripped.to_string();
        }
    }
    shown
}

/// Print `From <url>` and the per-ref lines of a fetch, nothing when nothing changed.
pub fn print_fetch_report(report: &rivet_core::remote_ops::FetchReport, quiet: bool) {
    if quiet || report.changes.is_empty() {
        return;
    }
    eprintln!("From {}", display_url(&report.url, true));
    for change in &report.changes {
        eprintln!("{}", change.fetch_line(report.width));
    }
}

/// The text Git puts in reflog lines for a command: its name and arguments as typed
/// (`pull --no-rebase`), without global options.
fn command_action(command: &str) -> String {
    let mut args = std::env::args().skip(1).peekable();
    let mut rest = Vec::new();
    let mut seen = false;
    while let Some(arg) = args.next() {
        if !seen {
            if arg == "--repo" {
                args.next();
            } else if arg == command {
                seen = true;
            }
            continue;
        }
        rest.push(arg);
    }
    if rest.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", rest.join(" "))
    }
}
