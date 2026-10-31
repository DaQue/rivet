//! `am`: apply a series of patches from a mailbox (the output of `format-patch`) as commits.
//!
//! Each message gives the author (`From:`), the author date (`Date:`), the commit message (the
//! subject without its `[PATCH …]` tag, then the body up to the `---` line) and the diff. The diff
//! is applied to the index and the work tree together, like `git apply --index`, and committed
//! with the current identity as committer.
//!
//! A patch that does not apply stops the run with Git's exit code (128), leaving `.git/rebase-apply`
//! (Git's directory name for it) with the remaining messages; `--continue` commits what was fixed
//! up by hand, `--skip` drops the patch and `--abort` restores the branch.
//!
//! `--3way` falls back, when a patch does not apply, to merging the post-image of the blobs named on
//! its `index` lines into the current tree, as git does; the conflict is then stopped like any other.
//!
//! Not implemented: `--signoff`, `--keep`, `--directory`, `--whitespace`, scissors,
//! mailbox formats other than plain mbox, RFC 2047 encoded headers.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::ops::HeadTarget;
use rivet_core::{ObjectId, ObjectKind, Repository, Signature};

use super::patch::{ApplyOptions, apply_to_repo, parse};
use crate::cli::GlobalArgs;

/// Arguments of `am`.
#[derive(Debug, clap::Args)]
pub struct AmArgs {
    /// Mailbox files (default: standard input).
    #[arg(value_name = "MBOX")]
    pub mbox: Vec<PathBuf>,

    /// Commit what was resolved by hand and go on.
    #[arg(long = "continue", short = 'r', alias = "resolved")]
    pub continue_am: bool,

    /// Drop the current patch and go on.
    #[arg(long = "skip")]
    pub skip: bool,

    /// Restore the branch to where it was before `am`.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Show the message (`raw`, the default) or the patch (`diff`) being applied.
    #[arg(
        long = "show-current-patch",
        value_name = "raw|diff",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "raw"
    )]
    pub show_current_patch: Vec<String>,

    /// Do not run the `applypatch-msg` and `pre-applypatch` hooks.
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,

    /// When a patch does not apply, merge it with the blobs its `index` lines name.
    #[arg(short = '3', long = "3way", overrides_with = "no_three_way")]
    pub three_way: bool,

    /// Do not fall back on a three-way merge.
    #[arg(long = "no-3way", overrides_with = "three_way")]
    pub no_three_way: bool,

    /// Stage the files `rerere` resolves from a recorded resolution.
    #[arg(long = "rerere-autoupdate", overrides_with = "no_rerere_autoupdate")]
    pub rerere_autoupdate: bool,

    /// Leave the files `rerere` resolves unstaged.
    #[arg(long = "no-rerere-autoupdate", overrides_with = "rerere_autoupdate")]
    pub no_rerere_autoupdate: bool,
}

fn dir_of(repository: &Repository) -> PathBuf {
    repository.git_dir().join("rebase-apply")
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string()
}

/// One parsed message.
struct Message {
    author: Signature,
    subject: String,
    body: Vec<u8>,
    patch: Vec<u8>,
}

fn split_mbox(text: &[u8]) -> Vec<Vec<u8>> {
    let mut messages: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut previous_blank = true;
    for line in text.split_inclusive(|b| *b == b'\n') {
        if line.starts_with(b"From ") && previous_blank {
            if let Some(done) = current.take() {
                messages.push(done);
            }
            // git's mailsplit keeps the `From ` line in the stored message.
            current = Some(line.to_vec());
            previous_blank = false;
            continue;
        }
        if let Some(message) = &mut current {
            message.extend_from_slice(line);
        }
        previous_blank = line == b"\n" || line == b"\r\n";
    }
    if let Some(done) = current {
        messages.push(done);
    }
    messages
}

/// `Tue, 14 Nov 2023 23:15:20 +0100` to (epoch seconds, offset minutes).
fn parse_rfc2822(text: &str) -> Option<(i64, i32)> {
    let text = text.trim();
    let text = text.split_once(',').map(|(_, rest)| rest).unwrap_or(text);
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: i64 = parts[0].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m.eq_ignore_ascii_case(parts[1]))? as i64
        + 1;
    let year: i64 = parts[2].parse().ok()?;
    let mut clock = parts[3].split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;
    let zone = parts[4];
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let digits = zone.trim_start_matches(['+', '-']);
    if digits.len() != 4 {
        return None;
    }
    let offset: i32 =
        sign * (digits[..2].parse::<i32>().ok()? * 60 + digits[2..].parse::<i32>().ok()?);
    // Days since 1970-01-01 (civil-from-days inverse).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let local = days * 86400 + hour * 3600 + minute * 60 + second;
    Some((local - offset as i64 * 60, offset))
}

fn strip_subject_tags(subject: &str) -> String {
    let mut text = subject.trim();
    loop {
        if let Some(rest) = text
            .strip_prefix("Re:")
            .or_else(|| text.strip_prefix("re:"))
        {
            text = rest.trim_start();
        } else if text.starts_with('[')
            && let Some(end) = text.find(']')
            && text[..end].contains("PATCH")
        {
            text = text[end + 1..].trim_start();
        } else {
            break;
        }
    }
    text.to_string()
}

fn parse_message(raw: &[u8], fallback: &Signature) -> Result<Message> {
    let split = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|p| p + 2)
        .unwrap_or(raw.len());
    let (head, rest) = raw.split_at(split);
    // Unfold the headers.
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.lines() {
        let line = String::from_utf8_lossy(line).into_owned();
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = headers.last_mut()
        {
            last.1.push(' ');
            last.1.push_str(line.trim());
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let from = header("from").unwrap_or_default();
    let (name, email) = match (from.find('<'), from.rfind('>')) {
        (Some(open), Some(close)) if open < close => (
            from[..open].trim().trim_matches('"').to_string(),
            from[open + 1..close].to_string(),
        ),
        _ => (from.clone(), from.clone()),
    };
    if !from.is_ascii() || header("subject").is_some_and(|s| s.contains("=?")) {
        anyhow::bail!("am: encoded or non-ASCII headers are not implemented");
    }
    let (when, tz) = header("date")
        .and_then(|d| parse_rfc2822(&d))
        .unwrap_or((fallback.when, fallback.tz_offset_minutes));
    let author = Signature {
        name: BString::from(name),
        email: BString::from(email),
        when,
        tz_offset_minutes: tz,
    };
    let subject = strip_subject_tags(&header("subject").unwrap_or_default());

    // The body ends where the patch starts: the `---` line, or the first `diff` header.
    let mut body: Vec<u8> = Vec::new();
    let mut patch_start = rest.len();
    let mut offset = 0;
    for line in rest.split_inclusive(|b| *b == b'\n') {
        if line.starts_with(b"---") && (line.len() == 4 || line[3] == b' ' || line[3] == b'\n') {
            patch_start = offset;
            break;
        }
        if line.starts_with(b"diff --git ") || line.starts_with(b"Index: ") {
            patch_start = offset;
            break;
        }
        body.extend_from_slice(line);
        offset += line.len();
    }
    let body = body.trim().to_vec();
    Ok(Message {
        author,
        subject,
        body,
        patch: rest[patch_start..].to_vec(),
    })
}

fn commit_message(message: &Message) -> Vec<u8> {
    let mut out = message.subject.clone().into_bytes();
    out.push(b'\n');
    if !message.body.is_empty() {
        out.push(b'\n');
        out.extend_from_slice(&message.body);
        out.push(b'\n');
    }
    out
}

fn hints(with_show: bool) {
    if with_show {
        eprintln!("hint: Use 'git am --show-current-patch=diff' to see the failed patch");
    }
    eprintln!("hint: When you have resolved this problem, run \"git am --continue\".");
    eprintln!("hint: If you prefer to skip this patch, run \"git am --skip\" instead.");
    eprintln!("hint: To restore the original branch and stop patching, run \"git am --abort\".");
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

/// Handler for `rivet am`.
///
/// # Exit codes
///
/// `0` all applied; `128` a patch did not apply (the run is stopped and resumable) or nothing to
/// resume.
pub fn run_am(global: &GlobalArgs, args: &AmArgs) -> Result<i32> {
    let mut repository = super::open(global)?;
    let dir = dir_of(&repository);
    super::rerere::set_autoupdate_flag(match (args.rerere_autoupdate, args.no_rerere_autoupdate) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    });
    if let Some(mode) = args.show_current_patch.last() {
        return show_current_patch(args, mode, &dir);
    }
    let controls = [args.continue_am, args.skip, args.abort]
        .iter()
        .filter(|on| **on)
        .count();
    if controls > 1 {
        anyhow::bail!("options '--continue', '--skip' and '--abort' cannot be used together");
    }
    if controls == 1 {
        if !dir.join("applying").exists() {
            eprintln!("fatal: Resolve operation not in progress, we are not resuming.");
            return Ok(128);
        }
        if args.abort {
            super::rerere::rerere_clear(&repository)?;
            return abort(&mut repository, &dir);
        }
        if args.skip {
            super::rerere::rerere_clear(&repository)?;
            let head = repository.head()?.oid();
            if let Some(head) = head {
                let tree = repository.commit_tree(head)?;
                repository.reset_hard_to_tree(tree)?;
            }
            let next: usize = read(&dir, "next").parse().unwrap_or(1);
            write(&dir, "next", &format!("{}\n", next + 1))?;
            return run_patches(global, &mut repository, &dir, args.no_verify);
        }
        return resume(global, &mut repository, &dir, args.no_verify);
    }

    if dir.exists() {
        eprintln!(
            "fatal: previous rebase directory .git/rebase-apply still exists but mbox given."
        );
        return Ok(128);
    }
    // Each file is its own mailbox; standard input is one more.
    let mut messages: Vec<Vec<u8>> = Vec::new();
    if args.mbox.is_empty() {
        let mut text = Vec::new();
        std::io::stdin().read_to_end(&mut text)?;
        messages.extend(split_mbox(&text));
    } else {
        for path in &args.mbox {
            match std::fs::read(path) {
                Ok(data) => messages.extend(split_mbox(&data)),
                Err(error) => {
                    let reason = if error.kind() == std::io::ErrorKind::NotFound {
                        "No such file or directory".to_string()
                    } else {
                        error.to_string()
                    };
                    eprintln!(
                        "fatal: could not open '{}' for reading: {reason}",
                        path.display()
                    );
                    return Ok(128);
                }
            }
        }
    }
    if messages.is_empty() {
        eprintln!("Patch format detection failed.");
        return Ok(128);
    }
    // The index must match HEAD.
    let head = repository.head()?.oid();
    if let Some(head) = head {
        let head_tree = repository.commit_tree(head)?;
        if repository.write_tree()? != head_tree {
            let mut options = rivet_core::StatusOptions::new();
            options.untracked_files = rivet_core::UntrackedMode::No;
            let status = repository.status(&options)?;
            let names: Vec<String> = status
                .staged
                .iter()
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect();
            eprintln!(
                "fatal: Dirty index: cannot apply patches (dirty: {})",
                names.join(" ")
            );
            return Ok(128);
        }
        rivet_core::write_orig_head(repository.git_dir(), head)?;
    }
    std::fs::create_dir_all(&dir)?;
    for (index, message) in messages.iter().enumerate() {
        std::fs::write(dir.join(format!("{:04}", index + 1)), message)?;
    }
    write(&dir, "next", "1\n")?;
    write(&dir, "last", &format!("{}\n", messages.len()))?;
    let configured = repository
        .config_value(BStr::new("am.threeway"))?
        .is_some_and(|v| {
            matches!(
                v.to_ascii_lowercase().as_slice(),
                b"true" | b"yes" | b"on" | b"1"
            )
        });
    let three_way = args.three_way || (configured && !args.no_three_way);
    write(&dir, "threeway", if three_way { "t\n" } else { "f\n" })?;
    run_patches(global, &mut repository, &dir, args.no_verify)
}

/// `am --show-current-patch[=raw|diff]`: print the stored message or the stored patch.
fn show_current_patch(args: &AmArgs, mode: &str, dir: &Path) -> Result<i32> {
    if !matches!(mode, "raw" | "diff") {
        eprintln!("error: invalid value for '--show-current-patch': '{mode}'");
        return Ok(129);
    }
    // git's command-mode options name the later option first.
    let shows: Vec<String> = std::env::args()
        .filter(|a| a.starts_with("--show-current-patch"))
        .collect();
    if let [.., earlier, later] = shows.as_slice() {
        eprintln!("error: options '{later}' and '{earlier}' cannot be used together");
        return Ok(129);
    }
    let spelled = format!("--show-current-patch={mode}");
    let spelled = if mode == "raw" && !std::env::args().any(|a| a == spelled) {
        "--show-current-patch".to_string()
    } else {
        spelled
    };
    let others = [
        ("--continue", args.continue_am),
        ("--skip", args.skip),
        ("--abort", args.abort),
    ];
    let position =
        |name: &str| std::env::args().position(|a| a == name || a == "-r" && name == "--continue");
    let shown_at = std::env::args().position(|a| a.starts_with("--show-current-patch"));
    for (name, on) in others {
        if on {
            let (later, earlier) = match (position(name), shown_at) {
                (Some(other), Some(shown)) if other > shown => (name, spelled.as_str()),
                _ => (spelled.as_str(), name),
            };
            eprintln!("error: options '{later}' and '{earlier}' cannot be used together");
            return Ok(129);
        }
    }
    if !dir.join("applying").exists() {
        eprintln!("fatal: Resolve operation not in progress, we are not resuming.");
        return Ok(128);
    }
    let file = if mode == "diff" {
        "patch".to_string()
    } else {
        read(dir, "next")
            .parse::<usize>()
            .map_or_else(|_| "0001".to_string(), |n| format!("{n:04}"))
    };
    let bytes = std::fs::read(dir.join(&file))
        .map_err(|e| anyhow::anyhow!("could not read '{}': {e}", dir.join(&file).display()))?;
    std::io::stdout().write_all(&bytes)?;
    Ok(0)
}

fn write(dir: &Path, name: &str, text: &str) -> Result<()> {
    std::fs::write(dir.join(name), text)
        .map_err(|e| anyhow::anyhow!("could not write '{}': {e}", dir.join(name).display()))
}

/// Run one of the hooks of `am`. None of them gets an index file or an editor of its own.
fn am_hook(repository: &Repository, name: &str, args: &[&std::ffi::OsStr]) -> Option<i32> {
    let mut hook = rivet_core::hooks::Hook::new(repository, name);
    for arg in args {
        hook = hook.arg(arg);
    }
    hook.run()
}

/// Apply the messages from `next` to `last`.
fn run_patches(
    global: &GlobalArgs,
    repository: &mut Repository,
    dir: &Path,
    no_verify: bool,
) -> Result<i32> {
    let last: usize = read(dir, "last").parse().unwrap_or(0);
    loop {
        let next: usize = read(dir, "next").parse().unwrap_or(1);
        if next > last {
            break;
        }
        let stored = std::fs::read(dir.join(format!("{next:04}")))?;
        let raw = match stored.starts_with(b"From ") {
            true => stored
                .iter()
                .position(|b| *b == b'\n')
                .map_or(&stored[..0], |at| &stored[at + 1..]),
            false => &stored[..],
        };
        let fallback = repository.author_signature()?;
        let message = parse_message(raw, &fallback)?;
        let number = format!("{next:04}");
        // Remember the message for `--continue`.
        write(dir, "applying", "")?;
        write(
            dir,
            "final-commit",
            &String::from_utf8_lossy(&commit_message(&message)),
        )?;
        write(
            dir,
            "author-script",
            &format!(
                "GIT_AUTHOR_NAME='{}'\nGIT_AUTHOR_EMAIL='{}'\nGIT_AUTHOR_DATE='@{} {}'\n",
                message.author.name,
                message.author.email,
                message.author.when,
                tz_text(message.author.tz_offset_minutes)
            ),
        )?;
        std::fs::write(dir.join("patch"), &message.patch)?;
        write(dir, "msg", &format!("{}\n", message.subject))?;
        if let Some(head) = repository.head()?.oid() {
            write(dir, "abort-safety", &format!("{head}\n"))?;
        }
        // `applypatch-msg` may rewrite the message in `final-commit`; a refusal stops before the
        // patch is applied.
        if !no_verify {
            let shown = super::snapshot::shown_path(repository, &dir.join("final-commit"));
            if am_hook(repository, "applypatch-msg", &[shown.as_os_str()]).is_some_and(|c| c != 0) {
                return Ok(1);
            }
        }
        if !global.quiet {
            println!("Applying: {}", message.subject);
        }

        let patches = match parse(&message.patch, 1) {
            Ok(patches) if !patches.is_empty() => patches,
            Ok(_) => {
                eprintln!("Patch is empty.");
                println!("Patch failed at {number} {}", message.subject);
                hints(false);
                return Ok(128);
            }
            Err(error) => {
                eprintln!("error: {error}");
                println!("Patch failed at {number} {}", message.subject);
                hints(true);
                return Ok(128);
            }
        };
        let options = ApplyOptions {
            index: true,
            ..ApplyOptions::default()
        };
        match apply_to_repo(repository, &patches, options) {
            Ok(warnings) => {
                for line in warnings {
                    eprintln!("{line}");
                }
            }
            Err(lines) if read(dir, "threeway") == "t" => {
                // The direct attempt's diagnostics are not shown when the merge may still work.
                drop(lines);
                if let Some(code) =
                    fall_back_threeway(global, repository, &patches, &message.subject, &number)?
                {
                    return Ok(code);
                }
            }
            Err(lines) => {
                for line in lines {
                    eprintln!("{line}");
                }
                hints(true);
                println!("Patch failed at {number} {}", message.subject);
                return Ok(128);
            }
        }
        if !no_verify && am_hook(repository, "pre-applypatch", &[]).is_some_and(|c| c != 0) {
            return Ok(1);
        }
        let final_message = std::fs::read(dir.join("final-commit"))?;
        commit_current(
            repository,
            &message.author,
            &final_message,
            &message.subject,
        )?;
        am_hook(repository, "post-applypatch", &[]);
        write(dir, "next", &format!("{}\n", next + 1))?;
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(0)
}

/// Git's `fall_back_threeway`: rebuild the blobs the patch started from, apply the patch to them,
/// and merge the result into the current tree. `Some(code)` means the run stopped (the message is
/// out already), `None` that the merge was clean and the patch can be committed.
fn fall_back_threeway(
    global: &GlobalArgs,
    repository: &mut Repository,
    patches: &[super::patch::FilePatch],
    subject: &str,
    number: &str,
) -> Result<Option<i32>> {
    let stop = |lines: Vec<String>| {
        for line in lines {
            eprintln!("{line}");
        }
        hints(true);
        println!("Patch failed at {number} {subject}");
        Ok(Some(128))
    };
    let ancestor = match super::patch::build_ancestor(repository, patches) {
        Ok(ancestor) => ancestor,
        Err(lines) => return stop(lines),
    };
    if !global.quiet {
        println!("Using index info to reconstruct a base tree...");
        for line in ancestor.changed_lines(repository) {
            println!("{line}");
        }
    }
    let their_tree = match ancestor.apply_patches(repository, patches) {
        Ok(tree) => tree,
        Err(mut lines) => {
            lines.push(
                "error: Did you hand edit your patch?\nIt does not apply to blobs recorded in its index."
                    .to_string(),
            );
            return stop(lines);
        }
    };
    let base_tree = ancestor.base_tree(repository)?;
    if !global.quiet {
        println!("Falling back to patching base and 3-way merge...");
    }
    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(repository, "merge.renames");
    options.labels = (BString::from("HEAD"), BString::from(subject));
    options.base_label = BString::from("constructed merge base");
    if let Some(style) = super::config_conflict_style(repository) {
        options.conflict_style = style;
    }
    let result = match repository.merge_trees_into_worktree(Some(base_tree), their_tree, &options) {
        Ok(result) => result,
        Err(error) => {
            let code = super::branching::report_checkout_refusal(error, "merge")?;
            hints(true);
            println!("Patch failed at {number} {subject}");
            return Ok(Some(if code == 1 { 128 } else { code }));
        }
    };
    let ours_tree = repository.head_tree_oid()?.unwrap_or(base_tree);
    let auto_merged = if global.quiet {
        Vec::new()
    } else {
        super::branching::cleanly_content_merged_trees(
            repository,
            Some(base_tree),
            ours_tree,
            their_tree,
            &result,
        )?
    };
    let conflicted = super::branching::print_merge_report(&result, subject, &auto_merged);
    if conflicted.is_empty() {
        return Ok(None);
    }
    super::rerere::rerere_hook(repository)?;
    eprintln!("error: Failed to merge in the changes.");
    stop(Vec::new())
}

fn tz_text(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    format!("{sign}{:02}{:02}", minutes.abs() / 60, minutes.abs() % 60)
}

fn commit_current(
    repository: &mut Repository,
    author: &Signature,
    message: &[u8],
    subject: &str,
) -> Result<ObjectId> {
    let tree = repository.write_tree()?;
    let parent = repository.head()?.oid();
    if parent.is_none() {
        eprintln!("applying to an empty history");
    }
    let committer = repository.committer_signature()?;
    let mut builder = rivet_core::CommitBuilder::new();
    builder.tree(tree);
    if let Some(parent) = parent {
        builder.parent(parent);
    }
    builder.author(author.clone());
    builder.committer(committer);
    builder.message(BStr::new(message));
    let body = builder.build(repository.format())?;
    let oid = repository.odb().write_raw(ObjectKind::Commit, &body)?;
    let reason = format!("am: {subject}");
    repository.move_head(oid, BStr::new(reason.as_bytes()))?;
    Ok(oid)
}

fn resume(
    global: &GlobalArgs,
    repository: &mut Repository,
    dir: &Path,
    no_verify: bool,
) -> Result<i32> {
    let subject = read(dir, "msg");
    if repository.index().has_conflicts() {
        println!("Applying: {subject}");
        println!(
            "You still have unmerged paths in your index.\nYou should 'git add' each file with resolved conflicts to mark them as such.\nYou might run `git rm` on a file to accept \"deleted by them\" for it."
        );
        hints(false);
        return Ok(128);
    }
    super::rerere::rerere_hook(repository)?;
    let tree = repository.write_tree()?;
    let head_tree = repository.head_tree_oid()?;
    if head_tree == Some(tree) {
        hints(false);
        println!("Applying: {subject}");
        println!(
            "No changes - did you forget to use 'git add'?\nIf there is nothing left to stage, chances are that something else\nalready introduced the same changes; you might want to skip this patch."
        );
        return Ok(128);
    }
    // The author recorded when the patch was read.
    let script = read(dir, "author-script");
    let value = |key: &str| -> String {
        script
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}='")))
            .and_then(|v| v.strip_suffix('\''))
            .unwrap_or_default()
            .to_string()
    };
    let date = value("GIT_AUTHOR_DATE");
    let mut parts = date.trim_start_matches('@').split_whitespace();
    let when: i64 = parts
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or_default();
    let zone = parts.next().unwrap_or("+0000");
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let minutes = sign
        * (zone[1..3].parse::<i32>().unwrap_or(0) * 60 + zone[3..5].parse::<i32>().unwrap_or(0));
    let author = Signature {
        name: BString::from(value("GIT_AUTHOR_NAME")),
        email: BString::from(value("GIT_AUTHOR_EMAIL")),
        when,
        tz_offset_minutes: minutes,
    };
    let message = std::fs::read(dir.join("final-commit"))?;
    if !global.quiet {
        println!("Applying: {subject}");
    }
    if !no_verify && am_hook(repository, "pre-applypatch", &[]).is_some_and(|c| c != 0) {
        return Ok(1);
    }
    commit_current(repository, &author, &message, &subject)?;
    am_hook(repository, "post-applypatch", &[]);
    let next: usize = read(dir, "next").parse().unwrap_or(1);
    write(dir, "next", &format!("{}\n", next + 1))?;
    run_patches(global, repository, dir, no_verify)
}

fn abort(repository: &mut Repository, dir: &Path) -> Result<i32> {
    let git_dir = repository.git_dir().to_path_buf();
    let orig = std::fs::read_to_string(git_dir.join("ORIG_HEAD")).unwrap_or_default();
    let safety = read(dir, "abort-safety");
    let current = repository
        .head()?
        .oid()
        .map(|o| o.to_string())
        .unwrap_or_default();
    if let Ok(orig) = ObjectId::from_hex(repository.format(), BStr::new(orig.trim().as_bytes())) {
        if safety.is_empty() || safety == current {
            let tree = repository.commit_tree(orig)?;
            repository.reset_hard_to_tree(tree)?;
            // Point the branch back at where it was, logging as `am --abort`.
            let who = repository.committer_signature()?;
            let head = repository.head()?;
            if let rivet_core::HeadState::Attached { branch, .. } = &head {
                if current != orig.to_string() {
                    repository.set_ref_logged(branch, orig, &who, BStr::new(b"am --abort"))?;
                }
                let entry = rivet_core::ReflogEntry::new(
                    ObjectId::from_hex(repository.format(), BStr::new(current.as_bytes()))
                        .unwrap_or(orig),
                    orig,
                    who,
                    BString::from("am --abort"),
                );
                repository
                    .refs()
                    .append_reflog(&rivet_core::RefName::head(), &entry)?;
            } else {
                repository.point_head(HeadTarget::Detached(orig), BStr::new(b"am --abort"))?;
            }
        } else {
            eprintln!(
                "warning: You seem to have moved HEAD since the last 'am' failure.\nNot rewinding to ORIG_HEAD"
            );
        }
    }
    std::fs::remove_dir_all(dir)?;
    Ok(0)
}
