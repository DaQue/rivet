//! `bisect`: binary search through history for the commit that introduced a change.
//!
//! The state lives where Git keeps it, so that `status` and Git itself can read it: the refs
//! `refs/bisect/<bad>` and `refs/bisect/<good>-<id>` / `refs/bisect/skip-<id>`, and the files
//! `BISECT_START`, `BISECT_LOG`, `BISECT_TERMS`, `BISECT_NAMES`, `BISECT_EXPECTED_REV` and
//! `BISECT_ANCESTORS_OK` in the git directory.
//!
//! The commit to test next is the one Git's `find_bisection` picks: with the commits of
//! `<bad> ^<good>...` numbered oldest first, each commit is weighted by how many of them it
//! reaches, the first one that splits the range in half is taken, and otherwise the one closest
//! to half. Skipped commits move the pick to a pseudo-random other commit, with Git's generator.
//!
//! Not implemented, and refused by name: `visualize`/`view`, `--first-parent`,
//! `--reset-when-found`, and path arguments to `start` (which need history simplification).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use bstr::BStr;
use rivet_core::revwalk::{WalkOptions, walk_oids};
use rivet_core::{ObjectId, RefName, RefTarget, Repository};

use crate::cli::GlobalArgs;

/// Arguments of `bisect`: the subcommand and everything after it, in Git's own grammar (the
/// words `bad` and `good` can be replaced by terms of the user's choosing, so the grammar is not
/// a fixed set of clap subcommands).
#[derive(Debug, clap::Args)]
pub struct BisectArgs {
    /// `start`, `bad`/`new`, `good`/`old`, `skip`, `next`, `reset`, `log`, `replay`, `run`,
    /// `terms`, or a term chosen with `--term-bad`/`--term-good`, then its arguments.
    #[arg(value_name = "SUBCOMMAND", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = "usage: git bisect start [--term-(bad|new)=<term-new> --term-(good|old)=<term-old>]
                        [--no-checkout] [--first-parent] [--reset-when-found[=<where>]] [<bad> [<good>...]] [--] [<pathspec>...]
   or: git bisect (bad|new|<term-new>) [<rev>]
   or: git bisect (good|old|<term-old>) [<rev>...]
   or: git bisect terms [--term-(good|old) | --term-(bad|new)]
   or: git bisect skip [(<rev>|<range>)...]
   or: git bisect next
   or: git bisect reset [<commit>]
   or: git bisect (visualize|view)
   or: git bisect replay <logfile>
   or: git bisect log
   or: git bisect run [--reset-when-found[=<where>]] <cmd> [<arg>...]
   or: git bisect help
";

/// Git's `BISECT_*` exit statuses.
const FAILED: i32 = 1;
const ONLY_SKIPPED_LEFT: i32 = 2;
const MERGE_BASE_CHECK: i32 = 3;

#[derive(Clone)]
struct Terms {
    bad: String,
    good: String,
}

impl Terms {
    fn read(repository: &Repository) -> Terms {
        let text =
            std::fs::read_to_string(repository.git_dir().join("BISECT_TERMS")).unwrap_or_default();
        let mut lines = text.lines();
        match (lines.next(), lines.next()) {
            (Some(bad), Some(good)) => Terms {
                bad: bad.to_string(),
                good: good.to_string(),
            },
            _ => Terms {
                bad: "bad".to_string(),
                good: "good".to_string(),
            },
        }
    }
}

fn git_file(repository: &Repository, name: &str) -> PathBuf {
    repository.git_dir().join(name)
}

fn read_file(repository: &Repository, name: &str) -> String {
    std::fs::read_to_string(git_file(repository, name)).unwrap_or_default()
}

fn append_log(repository: &Repository, text: &str) -> Result<()> {
    use std::io::Write as _;
    let path = git_file(repository, "BISECT_LOG");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("couldn't open the file '{}': {e}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Git's `sq_quote_argv`: each argument single-quoted, preceded by a space.
fn quote_args(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        out.push_str(" '");
        out.push_str(&arg.replace('\'', "'\\''"));
        out.push('\'');
    }
    out
}

fn unquote(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let mut word = String::new();
        let mut current = c;
        loop {
            match current {
                '\'' => {
                    for q in chars.by_ref() {
                        if q == '\'' {
                            break;
                        }
                        word.push(q);
                    }
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                }
                other => word.push(other),
            }
            match chars.peek() {
                Some(next) if !next.is_whitespace() => current = chars.next().unwrap_or(' '),
                _ => break,
            }
        }
        out.push(word);
    }
    out
}

struct Known {
    bad: Option<ObjectId>,
    good: Vec<ObjectId>,
    skipped: Vec<ObjectId>,
}

/// The `refs/bisect/*` refs, read the way `read_bisect_refs` does.
fn read_refs(repository: &Repository, terms: &Terms) -> Result<Known> {
    let mut known = Known {
        bad: None,
        good: Vec::new(),
        skipped: Vec::new(),
    };
    let bad_name = format!("refs/bisect/{}", terms.bad);
    let good_prefix = format!("refs/bisect/{}-", terms.good);
    let mut entries = repository.refs().list(b"refs/bisect/")?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, target) in entries {
        let RefTarget::Direct(oid) = target else {
            continue;
        };
        let text = name.to_string();
        if text == bad_name {
            known.bad = Some(oid);
        } else if text.starts_with(&good_prefix) {
            known.good.push(oid);
        } else if text.starts_with("refs/bisect/skip-") {
            known.skipped.push(oid);
        }
    }
    known.skipped.sort();
    Ok(known)
}

/// Remove every trace of a bisection: the refs and the state files.
fn clean_state(repository: &Repository) -> Result<()> {
    for (name, _) in repository.refs().list(b"refs/bisect/")? {
        repository.refs().delete(&name)?;
    }
    for name in [
        "BISECT_START",
        "BISECT_LOG",
        "BISECT_EXPECTED_REV",
        "BISECT_ANCESTORS_OK",
        "BISECT_NAMES",
        "BISECT_RUN",
        "BISECT_TERMS",
        "BISECT_FIRST_PARENT",
        "BISECT_HEAD",
    ] {
        let _ = std::fs::remove_file(git_file(repository, name));
    }
    let _ = std::fs::remove_dir(repository.git_dir().join("refs/bisect"));
    Ok(())
}

fn started(repository: &Repository) -> bool {
    std::fs::metadata(git_file(repository, "BISECT_START")).is_ok_and(|m| m.len() > 0)
}

/// Handler for `rivet bisect`.
///
/// # Exit codes
///
/// `0`; `1` for a refused or failed step; `2` when only skipped commits are left; `3` when the
/// merge base of the good and bad commits is itself bad; `129` for a usage error.
pub fn run_bisect(global: &GlobalArgs, args: &BisectArgs) -> Result<i32> {
    let Some(sub) = args.args.first() else {
        eprintln!("fatal: need a command\n");
        eprintln!("{USAGE}");
        return Ok(129);
    };
    let rest = &args.args[1..];
    let repository = super::open(global)?;
    let terms = Terms::read(&repository);
    match sub.as_str() {
        "help" | "-h" | "--help" => {
            eprintln!("{USAGE}");
            Ok(129)
        }
        "start" => bisect_start(global, rest),
        "reset" => bisect_reset(global, rest),
        "log" => {
            if !started(&repository) {
                eprintln!("error: We are not bisecting.");
                return Ok(1);
            }
            let log = read_file(&repository, "BISECT_LOG");
            print!("{log}");
            Ok(0)
        }
        "next" => {
            if !started(&repository) {
                eprintln!("You need to start by \"git bisect start\"\n");
                return Ok(1);
            }
            bisect_next(global, &repository, &terms)
        }
        "terms" => bisect_terms(&repository, &terms, rest),
        "replay" => bisect_replay(global, rest),
        "run" => bisect_run(global, rest),
        "visualize" | "view" => anyhow::bail!("bisect: {sub} is not implemented"),
        "skip" => bisect_state(global, "skip", rest),
        word => {
            let has_terms =
                std::fs::metadata(git_file(&repository, "BISECT_TERMS")).is_ok_and(|m| m.len() > 0);
            let is_state = word == terms.bad
                || word == terms.good
                || (!has_terms && matches!(word, "bad" | "good" | "new" | "old"));
            if is_state {
                return bisect_state(global, word, rest);
            }
            if has_terms && matches!(word, "bad" | "good" | "new" | "old") {
                eprintln!(
                    "error: Invalid command: you're currently in a {}/{} bisect",
                    terms.bad, terms.good
                );
            }
            eprintln!("fatal: unknown command: '{word}'\n");
            eprintln!("{USAGE}");
            Ok(129)
        }
    }
}

fn bisect_terms(repository: &Repository, terms: &Terms, args: &[String]) -> Result<i32> {
    if !std::fs::metadata(git_file(repository, "BISECT_TERMS")).is_ok_and(|m| m.len() > 0) {
        eprintln!("error: no terms defined");
        return Ok(1);
    }
    match args.first().map(String::as_str) {
        None => {
            println!(
                "Your current terms are '{}' for the old state\nand '{}' for the new state.",
                terms.good, terms.bad
            );
            Ok(0)
        }
        Some("--term-good" | "--term-old") => {
            println!("{}", terms.good);
            Ok(0)
        }
        Some("--term-bad" | "--term-new") => {
            println!("{}", terms.bad);
            Ok(0)
        }
        Some(other) => {
            eprintln!("error: invalid argument {other} for 'git bisect terms'.");
            eprintln!("Supported options are: --term-good|--term-old and --term-bad|--term-new.");
            Ok(1)
        }
    }
}

fn write_terms(repository: &Repository, bad: &str, good: &str) -> Result<()> {
    std::fs::write(
        git_file(repository, "BISECT_TERMS"),
        format!("{bad}\n{good}\n"),
    )?;
    Ok(())
}

/// A `bisect_write`: set the ref for `state` and log it.
fn write_state(
    repository: &Repository,
    terms: &Terms,
    state: &str,
    oid: ObjectId,
    log_command: bool,
) -> Result<()> {
    let name = if state == terms.bad {
        format!("refs/bisect/{state}")
    } else {
        format!("refs/bisect/{state}-{oid}")
    };
    repository
        .refs()
        .set_direct(&RefName::from_bytes(name.as_bytes())?, oid, None)?;
    let subject = super::commit_subject(repository, &oid)?;
    let mut line = format!("# {state}: [{oid}] {subject}\n");
    if log_command {
        line.push_str(&format!("git bisect {state} {oid}\n"));
    }
    append_log(repository, &line)
}

fn bisect_start(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    start_impl(global, args, true)
}

fn start_impl(global: &GlobalArgs, args: &[String], auto: bool) -> Result<i32> {
    let repository = super::open(global)?;
    let mut terms = Terms {
        bad: "bad".to_string(),
        good: "good".to_string(),
    };
    let mut custom_terms = false;
    let mut no_checkout = false;
    let mut revs: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    let mut iter = args.iter().peekable();
    let mut after_dashes = false;
    while let Some(arg) = iter.next() {
        if after_dashes {
            paths.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => after_dashes = true,
            "--no-checkout" => no_checkout = true,
            "--first-parent" => anyhow::bail!("bisect: --first-parent is not implemented"),
            a if a.starts_with("--reset-when-found") => {
                anyhow::bail!("bisect: --reset-when-found is not implemented")
            }
            "--term-good" | "--term-old" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `{}' requires a value", &arg[2..]);
                    return Ok(129);
                };
                terms.good = value.clone();
                custom_terms = true;
            }
            "--term-bad" | "--term-new" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `{}' requires a value", &arg[2..]);
                    return Ok(129);
                };
                terms.bad = value.clone();
                custom_terms = true;
            }
            a if a.starts_with("--term-good=") || a.starts_with("--term-old=") => {
                terms.good = a
                    .split_once('=')
                    .map(|x| x.1.to_string())
                    .unwrap_or_default();
                custom_terms = true;
            }
            a if a.starts_with("--term-bad=") || a.starts_with("--term-new=") => {
                terms.bad = a
                    .split_once('=')
                    .map(|x| x.1.to_string())
                    .unwrap_or_default();
                custom_terms = true;
            }
            a if a.starts_with("--") => {
                eprintln!("error: unrecognized option: '{a}'");
                return Ok(1);
            }
            a => {
                if super::resolve_commit(&repository, a).is_ok() {
                    revs.push(a.to_string());
                } else {
                    eprintln!("fatal: '{a}' does not appear to be a valid revision");
                    return Ok(128);
                }
            }
        }
    }
    if !paths.is_empty() {
        anyhow::bail!("bisect: path arguments to start are not implemented");
    }
    if terms.bad == terms.good && custom_terms {
        eprintln!("error: please use two different terms");
        return Ok(1);
    }
    let mut resolved: Vec<ObjectId> = Vec::new();
    for rev in &revs {
        resolved.push(super::resolve_commit(&repository, rev)?);
    }

    // Where the user was: the branch name, or the detached commit.
    let head = repository.head()?;
    let Some(head_oid) = head.oid() else {
        eprintln!("error: bad HEAD - I need a HEAD");
        return Ok(1);
    };
    let start_text = read_file(&repository, "BISECT_START").trim().to_string();
    let start_head = if !start_text.is_empty() {
        // A restart: go back to where the first one began.
        if !no_checkout {
            drop(repository);
            let code = checkout_noisy(global, &start_text)?;
            if code != 0 {
                eprintln!(
                    "error: checking out '{start_text}' failed. Try 'git bisect start <valid-branch>'."
                );
                return Ok(1);
            }
            return finish_start(
                global,
                args,
                terms,
                custom_terms,
                no_checkout,
                resolved,
                start_text,
                head_oid,
                auto,
            );
        }
        start_text
    } else {
        match head.branch() {
            Some(branch) => branch.short_name().to_string(),
            None => head_oid.to_string(),
        }
    };
    drop(repository);
    finish_start(
        global,
        args,
        terms,
        custom_terms,
        no_checkout,
        resolved,
        start_head,
        head_oid,
        auto,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_start(
    global: &GlobalArgs,
    args: &[String],
    terms: Terms,
    custom_terms: bool,
    no_checkout: bool,
    revs: Vec<ObjectId>,
    start_head: String,
    head_oid: ObjectId,
    auto: bool,
) -> Result<i32> {
    let repository = super::open(global)?;
    let head_oid = if no_checkout {
        head_oid
    } else {
        repository.head()?.oid().unwrap_or(head_oid)
    };
    clean_state(&repository)?;
    std::fs::write(
        git_file(&repository, "BISECT_START"),
        format!("{start_head}\n"),
    )?;
    std::fs::write(git_file(&repository, "BISECT_NAMES"), "\n")?;
    if no_checkout {
        std::fs::write(
            git_file(&repository, "BISECT_HEAD"),
            format!("{head_oid}\n"),
        )?;
    }
    // The terms file fixes the vocabulary: Git writes it when terms were chosen or revisions were
    // given, and otherwise leaves it to the first `bad`/`good`/`new`/`old` command.
    if custom_terms || !revs.is_empty() {
        write_terms(&repository, &terms.bad, &terms.good)?;
    }
    // The first revision is bad, the rest are good.
    for (i, oid) in revs.iter().enumerate() {
        let state = if i == 0 { &terms.bad } else { &terms.good };
        write_state(&repository, &terms, state, *oid, false)?;
    }
    append_log(
        &repository,
        &format!("git bisect start{}\n", quote_args(args)),
    )?;
    drop(repository);
    if auto { auto_next(global) } else { Ok(0) }
}

fn bisect_reset(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let repository = super::open(global)?;
    if args.len() > 1 {
        eprintln!("error: 'git bisect reset' requires either no argument or a commit");
        return Ok(1);
    }
    let branch = match args.first() {
        Some(commit) => {
            if super::resolve_commit(&repository, commit).is_err() {
                eprintln!("error: '{commit}' is not a valid commit");
                return Ok(1);
            }
            commit.clone()
        }
        None => {
            let start = read_file(&repository, "BISECT_START").trim().to_string();
            if start.is_empty() {
                // Nothing to go back to; leftover refs and files are still cleared.
                clean_state(&repository)?;
                return Ok(0);
            }
            start
        }
    };
    let no_checkout = git_file(&repository, "BISECT_HEAD").exists();
    drop(repository);
    if !no_checkout {
        let code = checkout_noisy(global, &branch)?;
        if code != 0 {
            eprintln!(
                "error: could not check out original HEAD '{branch}'. Try 'git bisect reset <commit>'."
            );
            return Ok(1);
        }
    }
    let repository = super::open(global)?;
    clean_state(&repository)?;
    Ok(0)
}

/// `git checkout <target> --`, with its usual messages.
fn checkout_noisy(global: &GlobalArgs, target: &str) -> Result<i32> {
    super::branching::run_checkout(global, &checkout_args(target))
}

/// `git checkout -q <target> --`.
fn checkout_quiet(global: &GlobalArgs, target: &str) -> Result<i32> {
    let quiet = GlobalArgs {
        repo: global.repo.clone(),
        verbose: global.verbose,
        quiet: true,
        no_pager: global.no_pager,
    };
    super::branching::run_checkout(&quiet, &checkout_args(target))
}

fn checkout_args(target: &str) -> super::branching::CheckoutArgs {
    super::branching::CheckoutArgs {
        target: Some(target.to_string()),
        paths: Vec::new(),
        orphan: None,
        create: None,
        force_create: None,
        detach: false,
        force: false,
        ours: false,
        theirs: false,
        overwrite_ignore: false,
        merge: false,
        source: None,
        track: false,
        dry_run: false,
        ignore_skip_worktree_bits: false,
    }
}

/// `skip`, the good term and the bad term: record the revisions and go on.
fn bisect_state(global: &GlobalArgs, state: &str, revs: &[String]) -> Result<i32> {
    state_impl(global, state, revs, true)
}

fn state_impl(global: &GlobalArgs, state: &str, revs: &[String], auto: bool) -> Result<i32> {
    let repository = super::open(global)?;
    if !started(&repository) {
        eprintln!("You need to start by \"git bisect start\"\n");
        return Ok(1);
    }
    let mut terms = Terms::read(&repository);
    let has_terms =
        std::fs::metadata(git_file(&repository, "BISECT_TERMS")).is_ok_and(|m| m.len() > 0);
    // The first of `bad`/`good` or `new`/`old` fixes the terms for the rest of the bisection.
    let state_name = if state == "skip" {
        "skip".to_string()
    } else if !has_terms {
        match state {
            "bad" | "good" => {
                write_terms(&repository, "bad", "good")?;
                terms = Terms {
                    bad: "bad".into(),
                    good: "good".into(),
                };
                state.to_string()
            }
            "new" | "old" => {
                write_terms(&repository, "new", "old")?;
                terms = Terms {
                    bad: "new".into(),
                    good: "old".into(),
                };
                state.to_string()
            }
            other => other.to_string(),
        }
    } else {
        state.to_string()
    };
    if state_name != "skip" && state_name != terms.bad && state_name != terms.good {
        eprintln!(
            "error: Invalid command: you're currently in a {}/{} bisect",
            terms.bad, terms.good
        );
        return Ok(1);
    }
    if revs.len() > 1 && state_name == terms.bad {
        eprintln!(
            "error: 'git bisect {}' can take only one argument.",
            terms.bad
        );
        return Ok(1);
    }
    let mut oids: Vec<ObjectId> = Vec::new();
    if revs.is_empty() {
        let bisect_head = read_file(&repository, "BISECT_HEAD");
        let oid = match ObjectId::from_hex(
            repository.format(),
            BStr::new(bisect_head.trim().as_bytes()),
        ) {
            Ok(oid) if !bisect_head.trim().is_empty() => Some(oid),
            _ => repository.head()?.oid(),
        };
        match oid {
            Some(oid) => oids.push(oid),
            None => {
                eprintln!("error: Bad rev input: HEAD");
                return Ok(1);
            }
        }
    }
    for rev in revs {
        if state_name == "skip" && rev.contains("..") {
            let mut range = rev.splitn(2, "..");
            let (from, to) = (range.next().unwrap_or(""), range.next().unwrap_or(""));
            let to = if to.is_empty() { "HEAD" } else { to };
            let (Ok(from), Ok(to)) = (
                super::resolve_commit(&repository, from),
                super::resolve_commit(&repository, to),
            ) else {
                eprintln!("error: Bad rev input: {rev}");
                return Ok(1);
            };
            let mut options = WalkOptions::new();
            options.exclude(from);
            oids.extend(walk_oids(repository.odb(), &[to], &options)?);
            continue;
        }
        match super::resolve_commit(&repository, rev) {
            Ok(oid) => oids.push(oid),
            Err(_) => {
                eprintln!("error: Bad rev input: {rev}");
                return Ok(1);
            }
        }
    }
    let expected = read_file(&repository, "BISECT_EXPECTED_REV");
    let mut verify_expected =
        ObjectId::from_hex(repository.format(), BStr::new(expected.trim().as_bytes())).ok();
    for oid in &oids {
        write_state(&repository, &terms, &state_name, *oid, true)?;
        // Replay writes refs and log only; it leaves the expected-revision bookkeeping alone.
        if auto
            && let Some(expected) = verify_expected
            && expected != *oid
        {
            let _ = std::fs::remove_file(git_file(&repository, "BISECT_ANCESTORS_OK"));
            let _ = std::fs::remove_file(git_file(&repository, "BISECT_EXPECTED_REV"));
            verify_expected = None;
        }
    }
    drop(repository);
    if auto { auto_next(global) } else { Ok(0) }
}

/// `bisect_auto_next`: print the status while a side is missing, otherwise pick the next commit.
fn auto_next(global: &GlobalArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let terms = Terms::read(&repository);
    let known = read_refs(&repository, &terms)?;
    if known.bad.is_none() || known.good.is_empty() {
        let text = if known.bad.is_some() {
            format!(
                "waiting for '{}' commit(s), '{}' commit known",
                terms.good, terms.bad
            )
        } else if !known.good.is_empty() {
            let n = known.good.len();
            format!(
                "waiting for '{}' commit, {n} '{}' commit{} known",
                terms.bad,
                terms.good,
                if n == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "waiting for both '{}' and '{}' commits",
                terms.good, terms.bad
            )
        };
        println!("status: {text}");
        append_log(&repository, &format!("# status: {text}\n"))?;
        return Ok(0);
    }
    bisect_next(global, &repository, &terms)
}

/// Git's `sqrti`, in single precision as in `bisect.c`.
fn sqrti(value: i32) -> i32 {
    if value == 0 {
        return 0;
    }
    let value = value as f32;
    let mut x = value;
    loop {
        let y = (x + value / x) / 2.0;
        let d = if y > x { y - x } else { x - y };
        x = y;
        if d < 0.5 {
            break;
        }
    }
    x as i32
}

/// Which of `count` remaining candidates `skip_away` takes: Git's pseudo-random pick.
fn skip_index(count: usize) -> usize {
    let count = count as i32;
    let seeded = count.wrapping_mul(1_103_515_245).wrapping_add(12345);
    let prn = ((seeded / 65536) as u32 % 32768) as i32;
    ((count * prn / 32768) * sqrti(prn) / sqrti(32768)) as usize
}

fn estimate_steps(all: usize) -> usize {
    if all < 3 {
        return 0;
    }
    let n = (usize::BITS - 1 - all.leading_zeros()) as usize;
    let e = 1usize << n;
    let x = all - e;
    if e < 3 * x { n } else { n - 1 }
}

fn bisect_next(global: &GlobalArgs, repository: &Repository, terms: &Terms) -> Result<i32> {
    let known = read_refs(repository, terms)?;
    let Some(bad) = known.bad else {
        eprintln!("error: a {} revision is needed", terms.bad);
        return Ok(1);
    };
    let no_checkout = git_file(repository, "BISECT_HEAD").exists();

    // Every good revision must be an ancestor of the bad one.
    if !known.good.is_empty() && !git_file(repository, "BISECT_ANCESTORS_OK").exists() {
        let code = check_merge_bases(global, repository, terms, &known, bad, no_checkout)?;
        if let Some(code) = code {
            return Ok(code);
        }
        std::fs::write(git_file(repository, "BISECT_ANCESTORS_OK"), "")?;
    }

    let odb = repository.odb();
    let mut options = WalkOptions::new();
    for good in &known.good {
        options.exclude(*good);
    }
    let newest_first = walk_oids(odb, &[bad], &options)?;
    let all_mode = !known.skipped.is_empty();
    let found = find_bisection(repository, &newest_first, all_mode)?;

    // Skipped commits are taken out of the candidates.
    let mut tried: Vec<ObjectId> = Vec::new();
    let mut candidates = found.candidates.clone();
    if !known.skipped.is_empty() {
        let mut filtered: Vec<ObjectId> = Vec::new();
        let mut skipped_first = false;
        for oid in candidates {
            if known.skipped.binary_search(&oid).is_ok() {
                skipped_first = true;
                tried.push(oid);
            } else {
                filtered.push(oid);
            }
        }
        // `skip_away`: once the best commit was skipped, a pseudo-random one is taken instead.
        if skipped_first {
            let count = filtered.len();
            if count > 0 {
                let index = skip_index(count);
                filtered = filtered.split_off(index.min(count - 1));
            }
            candidates = filtered;
        } else {
            candidates = filtered;
        }
        // Without a skipped first candidate the list is returned as it was, minus the skipped.
        if !skipped_first {
            tried.clear();
        }
    }

    let Some(&chosen) = candidates.first() else {
        if let Some(code) = only_skipped(repository, &tried, None, terms)? {
            return Ok(code);
        }
        println!("{bad} was both '{}' and '{}'", terms.good, terms.bad);
        return Ok(FAILED);
    };
    if found.all == 0 {
        eprintln!("No testable commit found.\nMaybe you started with bad path arguments?");
        return Ok(4);
    }

    if chosen == bad {
        if let Some(code) = only_skipped(repository, &tried, Some(bad), terms)? {
            return Ok(code);
        }
        println!("{bad} is the first '{}' commit", terms.bad);
        append_log(
            repository,
            &format!(
                "# first '{}' commit: [{bad}] {}\n",
                terms.bad,
                super::commit_subject(repository, &bad)?
            ),
        )?;
        show_diff_tree(global, repository, bad)?;
        return Ok(0);
    }

    let left = found.all - found.reaches - 1;
    let steps = estimate_steps(found.all);
    println!(
        "Bisecting: {left} revision{} left to test after this (roughly {steps} step{})",
        if left == 1 { "" } else { "s" },
        if steps == 1 { "" } else { "s" }
    );
    checkout_candidate(global, repository, chosen, no_checkout)
}

/// Check out the commit to test and name it: `[<id>] <subject>`.
fn checkout_candidate(
    global: &GlobalArgs,
    repository: &Repository,
    oid: ObjectId,
    no_checkout: bool,
) -> Result<i32> {
    std::fs::write(
        git_file(repository, "BISECT_EXPECTED_REV"),
        format!("{oid}\n"),
    )?;
    if no_checkout {
        std::fs::write(git_file(repository, "BISECT_HEAD"), format!("{oid}\n"))?;
    } else {
        let code = checkout_quiet(global, &oid.to_string())?;
        if code != 0 {
            return Ok(code);
        }
    }
    println!("[{oid}] {}", super::commit_subject(repository, &oid)?);
    Ok(0)
}

fn only_skipped(
    repository: &Repository,
    tried: &[ObjectId],
    bad: Option<ObjectId>,
    terms: &Terms,
) -> Result<Option<i32>> {
    if tried.is_empty() {
        return Ok(None);
    }
    println!(
        "There are only 'skip'ped commits left to test.\nThe first '{}' commit could be any of:",
        terms.bad
    );
    for oid in tried {
        println!("{oid}");
    }
    if let Some(bad) = bad {
        println!("{bad}");
    }
    println!("We cannot bisect more!");
    let mut log = String::from("# only skipped commits left to test\n");
    for oid in bad.iter().chain(tried.iter()) {
        log.push_str(&format!(
            "# possible first '{}' commit: [{oid}] {}\n",
            terms.bad,
            super::commit_subject(repository, oid)?
        ));
    }
    append_log(repository, &log)?;
    Ok(Some(ONLY_SKIPPED_LEFT))
}

/// `check_good_are_ancestors_of_bad`: `Some(code)` stops the bisection step with that status.
fn check_merge_bases(
    global: &GlobalArgs,
    repository: &Repository,
    terms: &Terms,
    known: &Known,
    bad: ObjectId,
    no_checkout: bool,
) -> Result<Option<i32>> {
    let odb = repository.odb();
    // The merge bases of the bad commit against all the good ones.
    let mut bases: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for good in &known.good {
        for base in rivet_core::merge_bases(odb, bad, *good).unwrap_or_default() {
            if seen.insert(base) {
                bases.push(base);
            }
        }
    }
    // Keep only the best of them, as `get_merge_bases_many` does.
    let mut best: Vec<ObjectId> = Vec::new();
    for base in &bases {
        let mut dominated = false;
        for other in &bases {
            if other != base && repository.is_ancestor(*base, *other)? {
                dominated = true;
                break;
            }
        }
        if !dominated {
            best.push(*base);
        }
    }
    for mb in best {
        if mb == bad {
            let bad_hex = bad.to_string();
            let good_hex = known
                .good
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            let expected = read_file(repository, "BISECT_EXPECTED_REV");
            if expected.trim() == bad_hex {
                if terms.bad == "bad" && terms.good == "good" {
                    eprintln!(
                        "The merge base {bad_hex} is bad.\nThis means the bug has been fixed between {bad_hex} and [{good_hex}]."
                    );
                } else if terms.bad == "new" && terms.good == "old" {
                    eprintln!(
                        "The merge base {bad_hex} is new.\nThe property has changed between {bad_hex} and [{good_hex}]."
                    );
                } else {
                    eprintln!(
                        "The merge base {bad_hex} is {}.\nThis means the first '{}' commit is between {bad_hex} and [{good_hex}].",
                        terms.bad, terms.good
                    );
                }
                return Ok(Some(MERGE_BASE_CHECK));
            }
            eprintln!(
                "Some {} revs are not ancestors of the {} rev.\ngit bisect cannot work properly in this case.\nMaybe you mistook {} and {} revs?",
                terms.good, terms.bad, terms.good, terms.bad
            );
            return Ok(Some(FAILED));
        } else if known.good.contains(&mb) {
            continue;
        } else if known.skipped.binary_search(&mb).is_ok() {
            eprintln!(
                "Warning: the merge base between {bad} and [{}] must be skipped.\nSo we cannot be sure the first {} commit is between {mb} and {bad}.\nWe continue anyway.",
                known
                    .good
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
                terms.bad
            );
        } else {
            println!("Bisecting: a merge base must be tested");
            let code = checkout_candidate(global, repository, mb, no_checkout)?;
            return Ok(Some(code));
        }
    }
    Ok(None)
}

struct Bisection {
    /// The commits to try, best first (just the best one unless skips are in play).
    candidates: Vec<ObjectId>,
    /// How many commits the best one reaches.
    reaches: usize,
    /// How many commits are in the range.
    all: usize,
}

/// Git's `find_bisection` over the range `newest_first` (the output of `rev-list <bad> ^<good>...`).
fn find_bisection(
    repository: &Repository,
    newest_first: &[ObjectId],
    keep_all: bool,
) -> Result<Bisection> {
    let list: Vec<ObjectId> = newest_first.iter().rev().copied().collect();
    let nr = list.len();
    if nr == 0 {
        return Ok(Bisection {
            candidates: Vec::new(),
            reaches: 0,
            all: 0,
        });
    }
    let index: HashMap<ObjectId, usize> = list.iter().enumerate().map(|(i, o)| (*o, i)).collect();
    let mut parents: Vec<Vec<usize>> = Vec::with_capacity(nr);
    for oid in &list {
        let all_parents = repository.commit_parents(*oid)?;
        parents.push(
            all_parents
                .iter()
                .filter_map(|p| index.get(p).copied())
                .collect(),
        );
    }

    let mut weights: Vec<i64> = vec![0; nr];
    let mut counted = 0usize;
    for (i, p) in parents.iter().enumerate() {
        match p.len() {
            0 => {
                weights[i] = 1;
                counted += 1;
            }
            1 => weights[i] = -1,
            _ => weights[i] = -2,
        }
    }
    let halfway = |weight: i64| -> bool { matches!(2 * weight - nr as i64, -1..=1) };

    // Merges first: their reach is counted the slow way.
    for i in 0..nr {
        if weights[i] != -2 {
            continue;
        }
        let mut seen: HashSet<usize> = HashSet::new();
        let mut stack = vec![i];
        while let Some(c) = stack.pop() {
            if seen.insert(c) {
                stack.extend(parents[c].iter().copied());
            }
        }
        weights[i] = seen.len() as i64;
        if !keep_all && halfway(weights[i]) {
            return Ok(Bisection {
                candidates: vec![list[i]],
                reaches: weights[i] as usize,
                all: nr,
            });
        }
        counted += 1;
    }
    // The rest follow from a parent whose reach is known.
    while counted < nr {
        let mut progressed = false;
        for i in 0..nr {
            if weights[i] >= 0 {
                continue;
            }
            let Some(q) = parents[i].iter().copied().find(|q| weights[*q] >= 0) else {
                continue;
            };
            weights[i] = weights[q] + 1;
            counted += 1;
            progressed = true;
            if !keep_all && halfway(weights[i]) {
                return Ok(Bisection {
                    candidates: vec![list[i]],
                    reaches: weights[i] as usize,
                    all: nr,
                });
            }
        }
        if !progressed {
            break;
        }
    }

    let distance = |w: i64| -> i64 { w.min(nr as i64 - w) };
    if !keep_all {
        let mut best = 0usize;
        let mut best_distance = -1i64;
        for (i, w) in weights.iter().enumerate() {
            let d = distance(*w);
            if d > best_distance {
                best = i;
                best_distance = d;
            }
        }
        return Ok(Bisection {
            candidates: vec![list[best]],
            reaches: weights[best] as usize,
            all: nr,
        });
    }
    let mut sorted: Vec<usize> = (0..nr).collect();
    sorted.sort_by(|a, b| {
        distance(weights[*b])
            .cmp(&distance(weights[*a]))
            .then_with(|| list[*a].cmp(&list[*b]))
    });
    let reaches = weights[sorted[0]] as usize;
    Ok(Bisection {
        candidates: sorted.into_iter().map(|i| list[i]).collect(),
        reaches,
        all: nr,
    })
}

/// `git diff-tree --pretty --stat --summary --cc <commit>`.
fn show_diff_tree(global: &GlobalArgs, repository: &Repository, commit: ObjectId) -> Result<()> {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    super::history::run_show(
        global,
        &super::history::ShowArgs {
            object: commit.to_string(),
            paths: Vec::new(),
            pretty: None,
            no_patch: true,
            context: None,
            format: None,
            abbrev: None,
            stat: false,
            name_only: false,
            name_status: false,
            date: None,
        },
    )?;
    let parents = repository.commit_parents(commit)?;
    let new_tree = repository.commit_tree(commit)?;
    if parents.len() <= 1 {
        let old_tree = match parents.first() {
            Some(parent) => Some(repository.commit_tree(*parent)?),
            None => None,
        };
        println!();
        super::print_diffstat(repository, old_tree, new_tree)?;
    }
    Ok(())
}

fn bisect_replay(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let Some(file) = args.first() else {
        eprintln!("error: no logfile given");
        return Ok(1);
    };
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("error: cannot read {file} for replaying: {error}");
            return Ok(1);
        }
    };
    let code = bisect_reset(global, &[])?;
    if code != 0 {
        return Ok(code);
    }
    for line in text.lines() {
        let Some(command) = line.strip_prefix("git bisect ") else {
            continue;
        };
        let words = unquote(command);
        let Some((sub, rest)) = words.split_first() else {
            continue;
        };
        let code = match sub.as_str() {
            "start" => start_impl(global, rest, true)?,
            "terms" => 0,
            state => state_impl(global, state, rest, false)?,
        };
        if code != 0 {
            return Ok(code);
        }
    }
    auto_next(global)
}

fn bisect_run(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    if args.is_empty() {
        eprintln!("error: 'git bisect run' failed: no command provided.");
        return Ok(1);
    }
    if args[0].starts_with("--reset-when-found") {
        anyhow::bail!("bisect: --reset-when-found is not implemented");
    }
    {
        let repository = super::open(global)?;
        if !started(&repository) {
            return Ok(1);
        }
    }
    loop {
        let repository = super::open(global)?;
        let terms = Terms::read(&repository);
        let known = read_refs(&repository, &terms)?;
        if known.bad.is_none() || known.good.is_empty() {
            eprintln!("error: bisect run cannot continue any more");
            return Ok(1);
        }
        drop(repository);
        println!("running{}", quote_args(args));
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{} \"$@\"", args[0]))
            .arg("sh")
            .args(&args[1..])
            .status();
        let Ok(status) = status else {
            eprintln!("error: bisect run could not run the command");
            return Ok(1);
        };
        let code = status.code().unwrap_or(128);
        if !(0..128).contains(&code) {
            eprintln!(
                "error: bisect run failed: exit code {code} from{} is < 0 or >= 128",
                quote_args(args)
            );
            return Ok(126);
        }
        let state = if code == 125 {
            "skip".to_string()
        } else if code == 0 {
            terms.good.clone()
        } else {
            terms.bad.clone()
        };
        // `bisect_state` prints what the step did; its status says whether to go on.
        let step = bisect_state(global, &state, &[])?;
        let repository = super::open(global)?;
        let log = read_file(&repository, "BISECT_LOG");
        let done = log
            .lines()
            .last()
            .is_some_and(|l| l.starts_with("# first "));
        if done {
            println!("bisect found first '{}' commit", terms.bad);
            return Ok(0);
        }
        if step != 0 {
            if step == ONLY_SKIPPED_LEFT {
                eprintln!("error: bisect run cannot continue any more");
            }
            return Ok(if step == FAILED { 1 } else { step });
        }
    }
}
