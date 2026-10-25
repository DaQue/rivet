//! `rebase --interactive`: the todo list, the editors and the squash/fixup message.
//!
//! # What is implemented
//!
//! `pick`, `reword`, `edit`, `squash`, `fixup` (with `-C` and `-c`) and `drop` (and Git's one-letter
//! forms), `noop`, `exec`, `break`, a
//! todo list edited by `GIT_SEQUENCE_EDITOR` / `sequence.editor` / the usual editor chain, and
//! `core.commentChar`. Leading `pick`s that would not change anything are fast-forwarded over, as Git
//! does, so an untouched list rewrites nothing.
//!
//! # What is refused
//!
//! `label`, `reset`, `merge` and `update-ref` lines in the todo
//! list are refused by name *before* anything is touched, like `--exec` without `-i`, `--root`,
//! `--rebase-merges`, `--autosquash`, `--edit-todo` and signing on the command line.
//!
//! # Caveats
//!
//! * **The editor sees the message plus Git's usual `#` footer, not the `git status` template** that
//!   `git commit` would append (the same gap as `merge --continue`).
//! * **An empty message from the editor aborts the whole rebase** back to the original tip, where Git
//!   stops with the commit pending. Nothing is lost; it is just more drastic than Git.

use std::path::Path;

use anyhow::Result;
use bstr::{BStr, ByteSlice};
use rivet_core::{HeadState, ObjectId, ObjectKind, Repository};

use super::{read_commit, read_state, write_state};

/// The commands a todo line can carry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Cmd {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
    Noop,
    Exec,
    Break,
    Label,
    Reset,
}

impl Cmd {
    /// The command a todo word names, if it is one this implementation runs.
    pub(super) fn parse(word: &str) -> Option<Cmd> {
        Some(match word {
            "pick" | "p" => Cmd::Pick,
            "reword" | "r" => Cmd::Reword,
            "edit" | "e" => Cmd::Edit,
            "squash" | "s" => Cmd::Squash,
            "fixup" | "f" => Cmd::Fixup,
            "drop" | "d" => Cmd::Drop,
            "noop" => Cmd::Noop,
            "exec" | "x" => Cmd::Exec,
            "break" | "b" => Cmd::Break,
            "label" | "l" => Cmd::Label,
            "reset" | "t" => Cmd::Reset,
            _ => return None,
        })
    }

    /// The full word, as written to the todo file and the reflog.
    pub(super) fn name(self) -> &'static str {
        match self {
            Cmd::Pick => "pick",
            Cmd::Reword => "reword",
            Cmd::Edit => "edit",
            Cmd::Squash => "squash",
            Cmd::Fixup => "fixup",
            Cmd::Drop => "drop",
            Cmd::Noop => "noop",
            Cmd::Exec => "exec",
            Cmd::Break => "break",
            Cmd::Label => "label",
            Cmd::Reset => "reset",
        }
    }

    /// `squash` and `fixup` meld into the commit before them.
    pub(super) fn is_fixup(self) -> bool {
        matches!(self, Cmd::Squash | Cmd::Fixup)
    }
}

/// The flag of a `fixup` line: `-C` takes the message of that commit instead, `-c` does the same and
/// opens the editor on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum Flag {
    #[default]
    None,
    Replace,
    Edit,
}

impl Flag {
    /// The flag a todo word spells.
    pub(super) fn parse(word: &str) -> Flag {
        match word {
            "-C" => Flag::Replace,
            "-c" => Flag::Edit,
            _ => Flag::None,
        }
    }

    /// The word in the todo list, with its leading space (empty for no flag).
    fn suffix(self) -> &'static str {
        match self {
            Flag::None => "",
            Flag::Replace => " -C",
            Flag::Edit => " -c",
        }
    }
}

/// The flag and the commit word of a todo line of `cmd`, the line split into words.
pub(super) fn flag_of(cmd: Cmd, words: &[&str]) -> (Flag, Option<String>) {
    let flag = match (cmd, words.get(1)) {
        (Cmd::Fixup, Some(word)) => Flag::parse(word),
        _ => Flag::None,
    };
    let at = if flag == Flag::None { 1 } else { 2 };
    (flag, words.get(at).map(|w| w.to_string()))
}

/// Whether the rebase in `dir` was started with `--interactive`.
pub(super) fn is_interactive(dir: &Path) -> bool {
    dir.join("rivet-interactive").exists()
}

/// The comment character of the todo list and of messages: `core.commentChar`, `#` by default.
pub(super) fn comment_char(repository: &Repository) -> char {
    repository
        .config_value(BStr::new(b"core.commentchar"))
        .ok()
        .flatten()
        .and_then(|v| {
            let text = String::from_utf8_lossy(&v).into_owned();
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c != 'a' => Some(c),
                _ => None,
            }
        })
        .unwrap_or('#')
}

const HELP: &str = "\
#
# Commands:
# p, pick <commit> = use commit
# r, reword <commit> = use commit, but edit the commit message
# e, edit <commit> = use commit, but stop for amending
# s, squash <commit> = use commit, but meld into previous commit
# f, fixup [-C | -c] <commit> = like \"squash\" but keep only the previous
#                    commit's log message, unless -C is used, in which case
#                    keep only this commit's message; -c is same as -C but
#                    opens the editor
# x, exec <command> = run command (the rest of the line) using shell
# b, break = stop here (continue rebase later with 'git rebase --continue')
# d, drop <commit> = remove commit
# l, label <label> = label current HEAD with a name
# t, reset <label> = reset HEAD to a label
# m, merge [-C <commit> | -c <commit>] <label> [# <oneline>]
#         create a merge commit using the original merge commit's
#         message (or the oneline, if no original merge commit was
#         specified); use -c <commit> to reword the commit message
# u, update-ref <ref> = track a placeholder for the <ref> to be updated
#                       to this position in the new commits. The <ref> is
#                       updated at the end of the rebase
#
# These lines can be re-ordered; they are executed from top to bottom.
#
# If you remove a line here THAT COMMIT WILL BE LOST.
#
# However, if you remove everything, the rebase will be aborted.
#
";

/// The footer of the todo list the sequence editor opens, with `comment` as the comment character.
fn todo_footer(comment: char, range: &str, onto: &str, count: usize) -> String {
    let plural = if count == 1 { "command" } else { "commands" };
    let text = format!("\n# Rebase {range} onto {onto} ({count} {plural})\n{HELP}");
    recomment(&text, comment)
}

/// Turn the `#` that starts a line into `comment`.
fn recomment(text: &str, comment: char) -> String {
    if comment == '#' {
        return text.to_string();
    }
    text.split_inclusive('\n')
        .map(|line| match line.strip_prefix('#') {
            Some(rest) => format!("{comment}{rest}"),
            None => line.to_string(),
        })
        .collect()
}

/// Run an editor on `file`: for the todo list the sequence editor first, then the usual chain
/// (`GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`). `Ok(false)` when the editor failed.
pub(super) fn run_editor(repository: &Repository, sequence: bool, file: &Path) -> Result<bool> {
    let editor = editor_command(repository, sequence);
    if editor == ":" {
        return Ok(true);
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(file)
        .status()?;
    Ok(status.success())
}

/// The editor command `run_editor` uses.
fn editor_command(repository: &Repository, sequence: bool) -> String {
    let config = |key: &str| {
        repository
            .config_value(BStr::new(key.as_bytes()))
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
    };
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let mut editor = None;
    if sequence {
        editor = env("GIT_SEQUENCE_EDITOR").or_else(|| config("sequence.editor"));
    }
    editor
        .or_else(|| env("GIT_EDITOR"))
        .or_else(|| config("core.editor"))
        .or_else(|| env("VISUAL"))
        .or_else(|| env("EDITOR"))
        .unwrap_or_else(|| "vi".to_string())
}

/// Strip comment lines and trailing blanks, squeeze runs of empty lines and trim the ends, as
/// `git commit --cleanup=strip` does.
pub(super) fn cleanup(message: &[u8], comment: char) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut blank_pending = false;
    for line in message.lines() {
        if line.starts_with(comment.to_string().as_bytes()) {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() {
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending {
            out.push(b'\n');
            blank_pending = false;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// A message for `git commit` to start from: the text, a blank line and Git's footer.
fn editor_text(message: &[u8], comment: char) -> Vec<u8> {
    let mut text = message.to_vec();
    if !text.ends_with(b"\n") {
        text.push(b'\n');
    }
    let footer = recomment(
        "\n# Please enter the commit message for your changes. Lines starting\n# with '#' will be ignored, and an empty message aborts the commit.\n#\n",
        comment,
    );
    text.extend_from_slice(footer.as_bytes());
    text
}

/// Let the user edit `message`; `None` when the editor failed or the result is empty.
pub(super) fn edit_message(repository: &Repository, message: &[u8]) -> Result<Option<Vec<u8>>> {
    let comment = comment_char(repository);
    let path = repository.git_dir().join("COMMIT_EDITMSG");
    std::fs::write(&path, editor_text(message, comment))?;
    if !run_editor(repository, false, &path)? {
        return Ok(None);
    }
    let edited = cleanup(&std::fs::read(&path)?, comment);
    Ok((!edited.is_empty()).then_some(edited))
}

/// One parsed todo line.
pub(super) struct Item {
    pub cmd: Cmd,
    /// `-C` / `-c` of a `fixup`.
    pub flag: Flag,
    /// `None` for `noop`, `exec` and `break`.
    pub oid: Option<ObjectId>,
    /// The shell command of an `exec` line, or the name a `label` / `reset` line carries.
    pub text: Option<String>,
}

impl Item {
    /// The line for the state files and the editor, with `name` standing for the commit.
    fn line(&self, repository: &Repository, name: impl Fn(&ObjectId) -> String) -> Result<String> {
        Ok(match (&self.oid, &self.text) {
            (Some(oid), _) => format!(
                "{}{} {} # {}\n",
                self.cmd.name(),
                self.flag.suffix(),
                name(oid),
                super::subject_of(repository, oid)?
            ),
            (None, Some(text)) => format!("{} {text}\n", self.cmd.name()),
            (None, None) if self.cmd == Cmd::Break => "break\n".to_string(),
            (None, None) => "noop\n".to_string(),
        })
    }
}

/// What the edited todo list turned out to be.
pub(super) enum Parsed {
    Items(Vec<Item>),
    /// The lines Git would call invalid: messages for stderr.
    Invalid(Vec<String>),
    /// A command Git has and this implementation does not.
    Refused(String),
}

/// Whether `name` can be the name of a `label`: it has to make a valid ref under `refs/rewritten/`.
pub(super) fn valid_label(name: &str) -> bool {
    name != "@"
        && rivet_core::RefName::from_bytes(format!("refs/rewritten/{name}").as_bytes()).is_ok()
}

/// Read a todo list. Comment lines and blank lines are skipped; every command is resolved to a
/// commit now, so the list can be saved with full ids.
pub(super) fn parse_todo(repository: &Repository, text: &str, comment: char) -> Parsed {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim_start_matches([' ', '\t']);
        if line.is_empty() || line.starts_with(comment) {
            continue;
        }
        let mut words = line.split_whitespace();
        let word = words.next().unwrap_or_default();
        let name = match word {
            "m" | "merge" => Some("merge"),
            "u" | "update-ref" => Some("update-ref"),
            _ => None,
        };
        if let Some(name) = name {
            return Parsed::Refused(format!(
                "rebase: '{name}' in the todo list is not implemented"
            ));
        }
        if matches!(word, "l" | "label" | "t" | "reset") {
            let cmd = Cmd::parse(word).unwrap_or(Cmd::Label);
            let target = line[word.len()..].trim();
            if target.is_empty() {
                errors.push(format!("missing arguments for {}", cmd.name()));
                errors.push(format!("invalid line {}: {raw}", number + 1));
            } else if cmd == Cmd::Label && !valid_label(target) {
                errors.push(format!("'{target}' is not a valid label"));
                errors.push(format!("invalid line {}: {raw}", number + 1));
            } else {
                items.push(Item {
                    cmd,
                    flag: Flag::None,
                    oid: None,
                    text: Some(target.to_string()),
                });
            }
            continue;
        }
        if matches!(word, "b" | "break") {
            let extra = line[word.len()..].trim_start();
            if extra.is_empty() {
                items.push(Item {
                    cmd: Cmd::Break,
                    flag: Flag::None,
                    oid: None,
                    text: None,
                });
            } else {
                let tail = stripped_tail(text, number, comment);
                errors.push(format!(
                    "break does not accept arguments: '{}\n{tail}'",
                    extra.trim_end()
                ));
                errors.push(format!("invalid line {}: {raw}", number + 1));
            }
            continue;
        }
        if matches!(word, "x" | "exec") {
            let command = line[word.len()..].trim();
            if command.is_empty() {
                errors.push("missing arguments for exec".to_string());
                errors.push(format!("invalid line {}: {raw}", number + 1));
            } else {
                items.push(Item {
                    cmd: Cmd::Exec,
                    flag: Flag::None,
                    oid: None,
                    text: Some(command.to_string()),
                });
            }
            continue;
        }
        let Some(cmd) = Cmd::parse(word) else {
            errors.push(format!("invalid command '{word}'"));
            errors.push(format!("invalid line {}: {raw}", number + 1));
            continue;
        };
        if cmd == Cmd::Noop {
            items.push(Item {
                cmd,
                flag: Flag::None,
                oid: None,
                text: None,
            });
            continue;
        }
        let Some(mut target) = words.next() else {
            errors.push(format!("missing arguments for {}", cmd.name()));
            errors.push(format!("invalid line {}: {raw}", number + 1));
            continue;
        };
        let mut flag = Flag::None;
        if cmd == Cmd::Fixup && Flag::parse(target) != Flag::None {
            flag = Flag::parse(target);
            target = words.next().unwrap_or_default();
        } else if cmd == Cmd::Fixup && target.len() > 2 && target.is_char_boundary(2) {
            // The flag glued to the commit: `fixup -C<commit>`.
            let (head, rest) = target.split_at(2);
            if Flag::parse(head) != Flag::None {
                flag = Flag::parse(head);
                target = rest;
            }
        }
        match crate::cmd::resolve_commit(repository, target) {
            Ok(oid) => items.push(Item {
                cmd,
                flag,
                oid: Some(oid),
                text: None,
            }),
            Err(_) => {
                errors.push(format!("could not parse '{target}'"));
                errors.push(format!("invalid line {}: {raw}", number + 1));
            }
        }
    }
    if errors.is_empty() {
        Parsed::Items(items)
    } else {
        Parsed::Invalid(errors)
    }
}

/// What Git quotes after a `break` that was given arguments: its list is cleaned up (comment lines
/// gone, trailing blanks trimmed, runs of blank lines collapsed) and the quote runs to the end of it.
fn stripped_tail(text: &str, number: usize, comment: char) -> String {
    let mut out: Vec<&str> = Vec::new();
    for raw in text.lines().skip(number + 1) {
        let line = raw.trim_end();
        if line.starts_with(comment)
            || (line.is_empty() && out.last().is_none_or(|l| l.is_empty()) && !out.is_empty())
        {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.iter().map(|l| format!("{l}\n")).collect()
}

/// The lines of the state files for `items`: `<command> <full id> # <subject>`.
pub(super) fn todo_lines(repository: &Repository, items: &[Item]) -> Result<String> {
    let mut out = String::new();
    for item in items {
        out.push_str(&item.line(repository, |oid| oid.to_string())?);
    }
    Ok(out)
}

/// What `begin` needs to know about the rebase being set up.
pub(super) struct Plan<'a> {
    pub head_state: &'a HeadState,
    pub orig_head: ObjectId,
    pub head_name: &'a str,
    pub upstream: ObjectId,
    pub onto: ObjectId,
    pub onto_name: &'a str,
    pub commits: &'a [(ObjectId, String)],
    pub autostash: Option<ObjectId>,
    /// Reorder `fixup!` / `squash!` commits next to their targets (`--autosquash`).
    pub autosquash: bool,
    /// The `--exec` commands, run after every commit (after the whole chain for `squash`/`fixup`).
    pub exec: &'a [String],
}

/// The `fixup! `, `squash! ` or `amend! ` prefix of a subject: the todo command it asks for and
/// the rest of the subject.
fn fixupish(subject: &str) -> Option<(&'static str, &str)> {
    [
        ("fixup! ", "fixup"),
        ("amend! ", "amend"),
        ("squash! ", "squash"),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| subject.strip_prefix(prefix).map(|rest| (kind, rest)))
}

/// Git's `todo_list_rearrange_squash`: every `fixup!` / `squash!` commit follows the commit its
/// subject names (an earlier subject exactly, else a commit id, else an earlier subject it is a
/// prefix of), behind the fixups already attached to that commit. Returns `(command, index)` in
/// todo order, or the reason this reorder is not reproduced (a wrong reorder rewrites history
/// silently, so those cases refuse): `amend!` targets need `fixup -C`, and a target that is itself
/// a fixup commit needs git's nested chains.
fn autosquash_order(
    repository: &Repository,
    commits: &[(ObjectId, String)],
) -> Result<Vec<(&'static str, usize)>, String> {
    let mut by_subject: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut chains: Vec<Vec<(&'static str, usize)>> = vec![Vec::new(); commits.len()];
    let mut attached = vec![false; commits.len()];
    for (i, (_, subject)) in commits.iter().enumerate() {
        let Some((kind, mut rest)) = fixupish(subject) else {
            by_subject.entry(subject.as_str()).or_insert(i);
            continue;
        };
        while let Some((_, inner)) = fixupish(rest.trim_start()) {
            rest = inner;
        }
        let rest = rest.trim_start();
        let target = if let Some(&j) = by_subject.get(rest) {
            Some(j)
        } else if let Ok(oid) = crate::cmd::resolve_commit(repository, rest)
            && let Some(j) = commits.iter().position(|(c, _)| *c == oid)
        {
            if j >= i {
                return Err(format!(
                    "'{subject}' names a later commit of the todo list, which is not reproduced"
                ));
            }
            Some(j)
        } else {
            commits[..i]
                .iter()
                .position(|(_, earlier)| earlier.starts_with(rest))
        };
        let Some(j) = target else { continue };
        if fixupish(&commits[j].1).is_some() {
            return Err(format!(
                "'{subject}' targets the fixup commit '{}', which is not reproduced",
                commits[j].1
            ));
        }
        if kind == "amend" {
            return Err(format!(
                "'{subject}' needs 'fixup -C', which is not implemented"
            ));
        }
        attached[i] = true;
        chains[j].push((kind, i));
    }
    let mut order = Vec::with_capacity(commits.len());
    for i in 0..commits.len() {
        if !attached[i] {
            order.push(("pick", i));
            order.extend(chains[i].iter().copied());
        }
    }
    Ok(order)
}

/// The hint Git prints after a bad todo list.
const FIX_HINT: &str = "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'.\nOr you can abort the rebase with 'git rebase --abort'.";

/// Start an interactive rebase: write the todo list, let the user edit it, then detach at the base
/// (past the leading picks that change nothing) and hand over to the runner.
pub(super) fn begin(
    global: &crate::cli::GlobalArgs,
    repository: &mut Repository,
    plan: &Plan<'_>,
    dir: &Path,
) -> Result<i32> {
    let comment = comment_char(repository);
    let order = if plan.autosquash {
        match autosquash_order(repository, plan.commits) {
            Ok(order) => order,
            Err(reason) => {
                if let Some(stash) = plan.autostash {
                    super::apply_autostash(repository, stash)?;
                }
                anyhow::bail!("rebase --autosquash: {reason}");
            }
        }
    } else {
        (0..plan.commits.len()).map(|i| ("pick", i)).collect()
    };
    std::fs::create_dir_all(dir)?;
    write_state(dir, "head-name", &format!("{}\n", plan.head_name))?;
    write_state(dir, "onto", &format!("{}\n", plan.onto))?;
    write_state(dir, "orig-head", &format!("{}\n", plan.orig_head))?;
    write_state(dir, "interactive", "")?;
    write_state(dir, "rivet-interactive", "")?;
    write_state(dir, "done", "")?;
    if let Some(stash) = plan.autostash {
        write_state(dir, "autostash", &format!("{stash}\n"))?;
    }

    let abbrev = |oid: &ObjectId| crate::cmd::history::abbrev(repository, oid, false);
    let mut text = String::new();
    if plan.commits.is_empty() {
        text.push_str("noop\n");
    }
    let mut commands = 0;
    for (n, (command, index)) in order.iter().enumerate() {
        let (oid, subject) = &plan.commits[*index];
        text.push_str(&format!("{command} {} # {subject}\n", abbrev(oid)));
        commands += 1;
        let chained = order
            .get(n + 1)
            .is_some_and(|(next, _)| matches!(*next, "fixup" | "squash"));
        if !chained {
            for command in plan.exec {
                text.push_str(&format!("exec {command}\n"));
                commands += 1;
            }
        }
    }
    let range = format!("{}..{}", abbrev(&plan.upstream), abbrev(&plan.orig_head));
    text.push_str(&todo_footer(
        comment,
        &range,
        &abbrev(&plan.onto),
        if plan.commits.is_empty() { 0 } else { commands },
    ));
    let todo_path = dir.join("git-rebase-todo");
    std::fs::write(&todo_path, text)?;

    // Anything that goes wrong before the head moves leaves the repository as it was.
    let give_up = |repository: &mut Repository, message: Option<String>| -> Result<i32> {
        std::fs::remove_dir_all(dir)?;
        if let Some(stash) = plan.autostash {
            super::apply_autostash(repository, stash)?;
        }
        match message {
            Some(message) => anyhow::bail!(message),
            None => Ok(1),
        }
    };
    if !run_editor(repository, true, &todo_path)? {
        return give_up(
            repository,
            Some("There was a problem with the editor".into()),
        );
    }
    let edited = std::fs::read_to_string(&todo_path)?;
    let parsed = match parse_todo(repository, &edited, comment) {
        Parsed::Refused(message) => return give_up(repository, Some(message)),
        other => other,
    };
    let mut items = Vec::new();
    let mut problem: Option<Vec<String>> = None;
    let mut raw_lines = String::new();
    match parsed {
        Parsed::Items(list) => items = list,
        Parsed::Invalid(errors) => {
            raw_lines = edited.clone();
            problem = Some(errors);
        }
        Parsed::Refused(_) => unreachable!("refusals return above"),
    }
    if problem.is_none() && items.is_empty() {
        eprintln!("error: nothing to do");
        return give_up(repository, None);
    }
    if problem.is_none() {
        let first = items
            .iter()
            .find(|i| i.cmd != Cmd::Drop && i.cmd != Cmd::Noop);
        if let Some(first) = first
            && first.cmd.is_fixup()
        {
            problem = Some(vec![format!(
                "cannot '{}' without a previous commit",
                first.cmd.name()
            )]);
            raw_lines = edited.clone();
        }
    }
    // Leading picks whose parent is already the head change nothing: step over them.
    let mut landing = plan.onto;
    let mut skipped = 0;
    if problem.is_none() {
        for item in &items {
            let Some(oid) = item.oid.filter(|_| item.cmd == Cmd::Pick) else {
                break;
            };
            let parents = repository.commit_parents(oid)?;
            if parents.as_slice() != [landing] {
                break;
            }
            landing = oid;
            skipped += 1;
        }
    }

    // Detach at the base.
    rivet_core::write_orig_head(repository.git_dir(), plan.orig_head)?;
    let tree = repository.commit_tree(landing)?;
    repository.reset_hard_to_tree(tree)?;
    let reason = format!("{} (start): checkout {}", super::action(), plan.onto_name);
    repository.point_head(
        rivet_core::ops::HeadTarget::Detached(landing),
        BStr::new(reason.as_bytes()),
    )?;
    let zero = ObjectId::zero(repository.format());
    super::rebase_hook(
        repository,
        "post-checkout",
        &[
            plan.head_state.oid().unwrap_or(zero).to_string(),
            landing.to_string(),
            "1".to_string(),
        ],
        None,
        super::hook_action(),
    );

    if let Some(errors) = problem {
        write_state(dir, "end", &format!("{}\n", items.len()))?;
        write_state(dir, "msgnum", "0\n")?;
        write_state(dir, "git-rebase-todo", &raw_lines)?;
        write_state(dir, "git-rebase-todo.backup", &raw_lines)?;
        for error in errors {
            eprintln!("error: {error}");
        }
        eprintln!("{FIX_HINT}");
        return Ok(1);
    }
    let all = todo_lines(repository, &items)?;
    let (done, rest): (Vec<&str>, Vec<&str>) = {
        let lines: Vec<&str> = all.lines().collect();
        (lines[..skipped].to_vec(), lines[skipped..].to_vec())
    };
    let join = |lines: &[&str]| lines.iter().map(|l| format!("{l}\n")).collect::<String>();
    write_state(dir, "done", &join(&done))?;
    write_state(dir, "git-rebase-todo", &join(&rest))?;
    write_state(dir, "git-rebase-todo.backup", &all)?;
    let count = items.iter().filter(|i| i.cmd != Cmd::Noop).count();
    write_state(dir, "end", &format!("{count}\n"))?;
    write_state(dir, "msgnum", &format!("{skipped}\n"))?;
    super::run_todo(global, repository, dir)
}

/// Remember that `oid` was rewritten; the pair is written once the commit it ended up in is known.
pub(super) fn note_pending(dir: &Path, oid: ObjectId) -> Result<()> {
    use std::io::Write as _;
    let mut list = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("rewritten-pending"))?;
    writeln!(list, "{oid}")?;
    Ok(())
}

/// Write `<old> <head>` for every pending commit to the rewritten list.
pub(super) fn flush_pending(repository: &Repository, dir: &Path) -> Result<()> {
    let pending = dir.join("rewritten-pending");
    let Ok(text) = std::fs::read_to_string(&pending) else {
        return Ok(());
    };
    if let Some(head) = repository.head()?.oid() {
        for old in text.lines() {
            let old = ObjectId::from_hex(repository.format(), BStr::new(old.as_bytes()))?;
            super::record_rewrite(dir, old, head)?;
        }
    }
    std::fs::remove_file(pending)?;
    Ok(())
}

/// Replace `HEAD` by a commit with the same parents and author but another tree and message
/// (`git commit --amend`), logging `<action>: <subject>`. With `Some(quiet)` the summary line
/// `git commit` prints is shown unless `quiet`.
pub(super) fn commit_amend(
    repository: &mut Repository,
    tree: ObjectId,
    message: &[u8],
    action: &str,
    summary: Option<bool>,
) -> Result<ObjectId> {
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("rebase: HEAD is unborn");
    };
    let old = read_commit(repository, &head)?;
    let committer = repository.committer_signature()?;
    let mut builder = rivet_core::CommitBuilder::new();
    builder.tree(tree);
    for parent in &old.parents {
        builder.parent(*parent);
    }
    builder.author(old.author.clone());
    builder.committer(committer.clone());
    builder.message(BStr::new(message));
    let body = builder.build(repository.format())?;
    let oid = repository.odb().write_raw(ObjectKind::Commit, &body)?;
    let subject = message.lines().next().unwrap_or_default();
    let reason = format!("{action}: {}", String::from_utf8_lossy(subject));
    repository.move_head(oid, BStr::new(reason.as_bytes()))?;
    crate::cmd::snapshot::commit_hook(repository, "post-commit", &[], super::hook_action());
    if let Some(false) = summary {
        println!(
            "[detached HEAD {}] {}",
            crate::cmd::short_oid(&oid),
            crate::cmd::commit_subject(repository, &oid)?
        );
        if old.author.name != committer.name || old.author.email != committer.email {
            println!(" Author: {} <{}>", old.author.name, old.author.email);
        }
        println!(
            " Date: {}",
            rivet_core::ops::format_git_date(old.author.when, old.author.tz_offset_minutes)
        );
        let old_tree = match old.parents.first() {
            Some(parent) => Some(repository.commit_tree(*parent)?),
            None => None,
        };
        crate::cmd::print_shortstat_summary(repository, old_tree, tree)?;
    }
    Ok(oid)
}

/// Stop the rebase because the editor gave no usable message: go back to where it started.
pub(super) fn give_up_on_message(repository: &mut Repository, dir: &Path) -> Result<i32> {
    eprintln!("Aborting commit due to empty commit message.");
    eprintln!(
        "error: rebase stopped; an empty message is not resumable here, so the rebase is aborted"
    );
    super::abort(repository, dir)?;
    Ok(1)
}

/// Reword `HEAD`: let the user edit its message and amend it. `None` when the message was empty.
pub(super) fn reword_head(
    global: &crate::cli::GlobalArgs,
    repository: &mut Repository,
) -> Result<bool> {
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("rebase: HEAD is unborn");
    };
    let old = read_commit(repository, &head)?;
    let Some(message) = edit_message(repository, &old.message)? else {
        return Ok(false);
    };
    let tree = repository.commit_tree(head)?;
    commit_amend(
        repository,
        tree,
        &message,
        &format!("{} (reword)", super::action()),
        Some(global.quiet),
    )?;
    Ok(true)
}

fn trimmed(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .trim_end_matches('\n')
        .to_string()
}

/// The sections of a squash message that are still in it, turned into "will be skipped" ones: the
/// header reworded and the body commented out. `text` is the message without its first line.
fn skip_sections(text: &str, c: char) -> String {
    let first = format!("{c} This is the 1st commit message:");
    let numbered = format!("{c} This is the commit message #");
    let mut out = String::new();
    let mut blanks = String::new();
    let mut commenting = false;
    let mut body_started = false;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        let is_header = bare.starts_with(&format!("{c} This is the"))
            || bare.starts_with(&format!("{c} The commit message #"))
            || bare.starts_with(&format!("{c} The 1st commit message"));
        if is_header {
            out.push_str(&blanks);
            blanks.clear();
            commenting = false;
            body_started = false;
            if bare == first {
                out.push_str(&format!("{c} The 1st commit message will be skipped:\n"));
                commenting = true;
            } else if let Some(n) = bare
                .strip_prefix(&numbered)
                .and_then(|n| n.strip_suffix(':'))
            {
                out.push_str(&format!("{c} The commit message #{n} will be skipped:\n"));
                commenting = true;
            } else {
                out.push_str(line);
            }
        } else if commenting && bare.is_empty() {
            blanks.push_str(line);
        } else if commenting {
            // Blank lines inside the body are commented, those between header and body stay plain.
            if body_started {
                out.push_str(&blanks.replace('\n', &format!("{c}\n")));
            } else {
                out.push_str(&blanks);
            }
            blanks.clear();
            body_started = true;
            out.push_str(&format!("{c} {bare}\n"));
        } else {
            out.push_str(&blanks);
            blanks.clear();
            out.push_str(line);
        }
    }
    out.push_str(&blanks);
    out
}

/// Add the commit `oid` to the squash message of the chain in progress, starting the chain from the
/// message of `HEAD` when it is the first link.
pub(super) fn chain_add(
    repository: &Repository,
    dir: &Path,
    oid: ObjectId,
    cmd: Cmd,
    flag: Flag,
) -> Result<()> {
    use std::io::Write as _;
    let c = comment_char(repository);
    let squash = dir.join("message-squash");
    let fixups = dir.join("current-fixups");
    let mut text = match std::fs::read_to_string(&squash) {
        Ok(text) => text,
        Err(_) => {
            let Some(head) = repository.head()?.oid() else {
                anyhow::bail!("rebase: HEAD is unborn");
            };
            let first = trimmed(&read_commit(repository, &head)?.message);
            write_state(dir, "message-fixup", &format!("{first}\n"))?;
            format!(
                "{c} This is a combination of 1 commits.\n{c} This is the 1st commit message:\n\n{first}\n"
            )
        }
    };
    // `-C` / `-c` replace the message so far, unless a `squash` is already part of the chain.
    let replaces = flag != Flag::None
        && !read_state(dir, "current-fixups")
            .lines()
            .any(|l| l.starts_with("squash"));
    let mut list = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&fixups)?;
    writeln!(list, "{}{} {oid}", cmd.name(), flag.suffix())?;
    let count = read_state(dir, "current-fixups").lines().count() + 1;
    let rest = text
        .split_once('\n')
        .map_or("", |(_, rest)| rest)
        .to_string();
    let rest = if replaces {
        skip_sections(&rest, c)
    } else {
        rest
    };
    text = format!("{c} This is a combination of {count} commits.\n{rest}");
    let message = trimmed(&read_commit(repository, &oid)?.message);
    if replaces {
        write_state(dir, "message-fixup", &format!("{message}\n"))?;
    }
    if cmd == Cmd::Fixup && !replaces && flag == Flag::None {
        let commented: String = message
            .lines()
            .map(|l| {
                if l.is_empty() {
                    format!("{c}\n")
                } else {
                    format!("{c} {l}\n")
                }
            })
            .collect();
        text.push_str(&format!(
            "\n{c} The commit message #{count} will be skipped:\n\n{commented}"
        ));
    } else {
        // Git comments out the `squash! …` / `fixup! …` subject of a squashed commit.
        let message = if fixupish(&message).is_some() {
            format!("{c} {message}")
        } else {
            message
        };
        text.push_str(&format!(
            "\n{c} This is the commit message #{count}:\n\n{message}\n"
        ));
    }
    write_state(dir, "message-squash", &text)
}

/// Whether the next command of the todo list melds into the commit being built.
pub(super) fn next_is_fixup(dir: &Path) -> bool {
    read_state(dir, "git-rebase-todo")
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .and_then(Cmd::parse)
        .is_some_and(Cmd::is_fixup)
}

/// Commit `tree` into the chain: an unedited combined message while more links follow, and at the
/// last one the edited combination (or, with only `fixup`s, the first message alone).
/// `Ok(false)` when the editor left no message.
pub(super) fn chain_commit(
    global: &crate::cli::GlobalArgs,
    repository: &mut Repository,
    dir: &Path,
    tree: ObjectId,
    cmd: Cmd,
    action: &str,
) -> Result<bool> {
    let text = std::fs::read(dir.join("message-squash"))?;
    if next_is_fixup(dir) {
        super::interactive::commit_amend(repository, tree, &text, action, None)?;
        return Ok(true);
    }
    let has_squash = read_state(dir, "current-fixups")
        .lines()
        .any(|l| l.starts_with("squash") || l.starts_with("fixup -c"));
    let _ = cmd;
    let message = if has_squash {
        match edit_message(repository, &text)? {
            Some(message) => message,
            None => return Ok(false),
        }
    } else {
        std::fs::read(dir.join("message-fixup"))?
    };
    commit_amend(
        repository,
        tree,
        &message,
        action,
        has_squash.then_some(global.quiet),
    )?;
    for name in ["message-squash", "message-fixup", "current-fixups"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    Ok(true)
}

/// After `--skip` of a `squash` or `fixup`: the links before it still have to become one commit.
/// `Ok(false)` when the editor left no message.
pub(super) fn finish_chain_after_skip(
    global: &crate::cli::GlobalArgs,
    repository: &mut Repository,
    dir: &Path,
) -> Result<bool> {
    let skipped = read_state(dir, "done")
        .lines()
        .last()
        .and_then(|l| l.split_whitespace().next())
        .and_then(Cmd::parse);
    if skipped.is_some_and(Cmd::is_fixup)
        && dir.join("message-squash").exists()
        && !next_is_fixup(dir)
    {
        let Some(head) = repository.head()?.oid() else {
            anyhow::bail!("rebase: HEAD is unborn");
        };
        let tree = repository.commit_tree(head)?;
        return chain_commit(
            global,
            repository,
            dir,
            tree,
            Cmd::Squash,
            "rebase (continue)",
        );
    }
    Ok(true)
}

/// Git refuses a list whose first command (past `drop` and `noop`) melds into a previous commit.
fn leading_fixup_error(items: &[Item]) -> Option<String> {
    items
        .iter()
        .find(|i| i.cmd != Cmd::Drop && i.cmd != Cmd::Noop)
        .filter(|first| first.cmd.is_fixup())
        .map(|first| format!("cannot '{}' without a previous commit", first.cmd.name()))
}

/// The footer of the todo list `--edit-todo` opens: the command help up to the warning about removed
/// lines, then what the editing is for.
fn edit_todo_footer(comment: char) -> String {
    let help = HELP.split("# However").next().unwrap_or(HELP);
    let text = format!(
        "{help}# You are editing the todo file of an ongoing interactive rebase.\n# To continue rebase after editing, run:\n#     git rebase --continue\n#\n"
    );
    recomment(&text, comment)
}

/// `rebase --edit-todo`: let the user edit what is left of the todo list. The list is saved with
/// full ids; a list with invalid lines is saved as typed and reported, to be fixed with another
/// `--edit-todo`. The pre-edit list (and the help) is kept as `git-rebase-todo.backup`.
pub(super) fn edit_todo(repository: &Repository, dir: &Path) -> Result<i32> {
    if !is_interactive(dir) {
        anyhow::bail!("rebase --edit-todo is only implemented for an interactive rebase");
    }
    let comment = comment_char(repository);
    let path = dir.join("git-rebase-todo");
    let original = read_state(dir, "git-rebase-todo");
    let footer = edit_todo_footer(comment);
    let abbrev = |oid: &ObjectId| crate::cmd::history::abbrev(repository, oid, false);
    let (backup, mut shown) = match parse_todo(repository, &original, comment) {
        Parsed::Refused(message) => anyhow::bail!(message),
        Parsed::Items(items) => {
            if let Some(error) = leading_fixup_error(&items) {
                eprintln!("error: {error}");
            }
            let mut shown = String::new();
            for item in &items {
                shown.push_str(&item.line(repository, abbrev)?);
            }
            (todo_lines(repository, &items)?, shown)
        }
        Parsed::Invalid(errors) => {
            for error in errors {
                eprintln!("error: {error}");
            }
            (original.clone(), original.clone())
        }
    };
    if !shown.is_empty() && !shown.ends_with('\n') {
        shown.push('\n');
    }
    write_state(dir, "git-rebase-todo.backup", &format!("{backup}{footer}"))?;
    std::fs::write(&path, format!("{shown}{footer}"))?;
    if !run_editor(repository, true, &path)? {
        eprintln!(
            "error: there was a problem with the editor '{}'",
            editor_command(repository, true)
        );
        return Ok(1);
    }
    let edited = std::fs::read_to_string(&path)?;
    match parse_todo(repository, &edited, comment) {
        Parsed::Items(items) => {
            if let Some(error) = leading_fixup_error(&items) {
                eprintln!("error: {error}");
                eprintln!("{FIX_HINT}");
                return Ok(1);
            }
            std::fs::write(&path, todo_lines(repository, &items)?)?;
            Ok(0)
        }
        Parsed::Invalid(errors) => {
            for error in errors {
                eprintln!("error: {error}");
            }
            eprintln!("{FIX_HINT}");
            Ok(1)
        }
        Parsed::Refused(message) => {
            std::fs::write(&path, original)?;
            anyhow::bail!(message)
        }
    }
}
