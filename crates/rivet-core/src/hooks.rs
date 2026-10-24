//! Git hooks: locating a hook script and running it the way Git does.
//!
//! # Caveats
//!
//! * **A hook is looked up, not configured.** `core.hooksPath` (a relative value is taken from the
//!   work tree root, where hooks run, or from the git directory in a bare repository) or
//!   `<common dir>/hooks` is searched for a file named like the hook. A missing file is no hook at
//!   all and is silent.
//! * **A file that is not executable is skipped, with a hint.** Git prints
//!   `hint: The '<path>' hook was ignored because it's not set as executable.` once per hook name
//!   and process, unless `advice.ignoredHook` is false.
//! * **Hooks run in the work tree root** (the git directory when bare), with stdin closed unless the
//!   caller provides some, and with **stdout redirected to stderr**: a hook's output never
//!   reaches the caller's stdout.
//! * **`GIT_PREFIX` is always set**, to the directory the command was started in relative to the
//!   work tree root (`sub/`, or empty at the root); the caller adds the other variables
//!   (`GIT_INDEX_FILE`, `GIT_EDITOR`, `GIT_REFLOG_ACTION`) that depend on the operation.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use bstr::{BStr, BString};

use crate::repo::Repository;

/// Hook names whose "not executable" hint has already been printed in this process.
static HINTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One invocation of a hook: its name, arguments, extra environment and optional stdin.
///
/// Build it with [`Hook::new`], add to it, then [`Hook::run`].
#[derive(Debug)]
pub struct Hook<'a> {
    repo: &'a Repository,
    name: &'a str,
    args: Vec<OsString>,
    env: Vec<(String, OsString)>,
    stdin: Option<Vec<u8>>,
}

impl<'a> Hook<'a> {
    /// A call to the hook `name` (`pre-commit`, `post-checkout`, …) of `repo`.
    pub fn new(repo: &'a Repository, name: &'a str) -> Self {
        Self {
            repo,
            name,
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
        }
    }

    /// Append one argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Set one environment variable for the hook.
    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.to_string(), value.as_ref().to_os_string()));
        self
    }

    /// Set `GIT_INDEX_FILE` to the repository's index, spelled relative to the work tree root when
    /// it is inside it (`.git/index`), as Git does for the commit hooks.
    pub fn with_index(self) -> Self {
        let index = self.repo.git_dir().join("index");
        let shown = self
            .repo
            .worktree_path()
            .and_then(|root| index.strip_prefix(root).ok().map(Path::to_path_buf))
            .unwrap_or(index);
        self.env("GIT_INDEX_FILE", shown)
    }

    /// Feed `data` to the hook's stdin; without this stdin is empty.
    pub fn stdin(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    /// Whether a runnable hook of this name exists (without printing the "not executable" hint).
    pub fn exists(&self) -> bool {
        find(self.repo, self.name, false).is_some()
    }

    /// Run the hook.
    ///
    /// Returns `None` when there is no (executable) hook, otherwise the hook's exit status; a hook
    /// killed by a signal reports `128 + signal`, as Git does, and one that cannot be started
    /// prints `fatal: cannot exec '<path>': <reason>` and reports `128`, like Git's forked child.
    ///
    /// # Caveats
    ///
    /// The caller's own buffered stdout should be flushed first when ordering matters; this
    /// function flushes the process's stdout and stderr itself.
    pub fn run(self) -> Option<i32> {
        let (path, shown) = find(self.repo, self.name, true)?;
        let root = self
            .repo
            .worktree_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.repo.git_dir().to_path_buf());
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        let mut command = Command::new(&path);
        command
            .args(&self.args)
            .current_dir(&root)
            .env("GIT_PREFIX", prefix(self.repo))
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        let spawn = |mut command: Command, data: &Option<Vec<u8>>| {
            command.stdin(if data.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
            match std::io::stderr().as_fd().try_clone_to_owned() {
                Ok(fd) => command.stdout(Stdio::from(fd)),
                Err(_) => command.stdout(Stdio::inherit()),
            };
            command.spawn()
        };
        let mut child = match spawn(command, &self.stdin) {
            Ok(child) => child,
            // A script without a `#!` line: Git retries it through the shell.
            Err(e) if e.raw_os_error() == Some(8) => {
                let mut sh = Command::new("sh");
                sh.arg(&path)
                    .args(&self.args)
                    .current_dir(&root)
                    .env("GIT_PREFIX", prefix(self.repo))
                    .envs(self.env.iter().map(|(k, v)| (k, v)));
                match spawn(sh, &self.stdin) {
                    Ok(child) => child,
                    Err(e) => return Some(cannot_exec(&shown, &e)),
                }
            }
            Err(e) => return Some(cannot_exec(&shown, &e)),
        };
        if let (Some(data), Some(mut pipe)) = (self.stdin, child.stdin.take()) {
            // The hook may exit without reading everything; a broken pipe is its answer.
            let _ = pipe.write_all(&data);
        }
        match child.wait() {
            Ok(status) => {
                use std::os::unix::process::ExitStatusExt;
                Some(
                    status
                        .code()
                        .or_else(|| status.signal().map(|s| 128 + s))
                        .unwrap_or(1),
                )
            }
            Err(e) => Some(cannot_exec(&shown, &e)),
        }
    }
}

/// What Git's forked child says when `exec` fails (a directory in place of the hook, a script the
/// kernel refuses): `fatal: cannot exec '<path>': <strerror>`, and a status of 128.
fn cannot_exec(shown: &str, error: &std::io::Error) -> i32 {
    let text = error.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text);
    eprintln!("fatal: cannot exec '{shown}': {text}");
    128
}

/// `GIT_PREFIX`: the current directory relative to the work tree root, with a trailing slash.
fn prefix(repo: &Repository) -> OsString {
    let (Some(root), Ok(cwd)) = (repo.worktree_path(), std::env::current_dir()) else {
        return OsString::new();
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    match cwd.strip_prefix(&root) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            let mut out = rel.as_os_str().to_os_string().into_vec();
            out.push(b'/');
            OsString::from_vec(out)
        }
        _ => OsString::new(),
    }
}

/// The directory hooks are looked up in.
fn hooks_dir(repo: &Repository) -> (PathBuf, String) {
    let configured = repo
        .config()
        .get_path(BStr::new("core.hooksPath"))
        .ok()
        .flatten()
        .filter(|value| !value.is_empty());
    match configured {
        Some(value) => {
            let path = PathBuf::from(OsStr::from_bytes(&value));
            let shown = path.display().to_string();
            if path.is_absolute() {
                (path, shown)
            } else {
                let base = repo
                    .worktree_path()
                    .unwrap_or_else(|| repo.git_dir())
                    .to_path_buf();
                (base.join(&path), shown)
            }
        }
        None => {
            let dir = repo.common_dir().join("hooks");
            let shown = repo
                .worktree_path()
                .and_then(|root| dir.strip_prefix(root).ok())
                .unwrap_or(&dir)
                .display()
                .to_string();
            (dir, shown)
        }
    }
}

/// Locate the hook `name`: its path and the spelling Git would print for it.
///
/// A file that exists but is not executable gives `None`, after the one-time hint when `hint` is
/// set and `advice.ignoredHook` allows it.
fn find(repo: &Repository, name: &str, hint: bool) -> Option<(PathBuf, String)> {
    let (dir, shown_dir) = hooks_dir(repo);
    let path = dir.join(name);
    // A directory is not skipped: Git finds it executable and then fails to run it.
    let metadata = std::fs::metadata(&path).ok()?;
    let shown = format!("{}/{name}", shown_dir.trim_end_matches('/'));
    if metadata.permissions().mode() & 0o111 == 0 {
        if hint {
            let advice = repo
                .config()
                .get_bool(BStr::new("advice.ignoredHook"))
                .ok()
                .flatten()
                .unwrap_or(true);
            let mut seen = HINTED.lock().unwrap_or_else(|e| e.into_inner());
            if advice && !seen.iter().any(|n| n == name) {
                seen.push(name.to_string());
                eprintln!(
                    "hint: The '{shown}' hook was ignored because it's not set as executable."
                );
                eprintln!(
                    "hint: You can disable this warning with `git config set advice.ignoredHook false`."
                );
            }
        }
        return None;
    }
    Some((path, shown))
}

/// The lines of a `post-rewrite` stdin: `<old> <new>` per rewritten commit.
///
/// A convenience for callers that hold the pairs as hex strings.
pub fn rewrite_lines(pairs: &[(String, String)]) -> Vec<u8> {
    let mut out = BString::default();
    for (old, new) in pairs {
        out.extend_from_slice(old.as_bytes());
        out.push(b' ');
        out.extend_from_slice(new.as_bytes());
        out.push(b'\n');
    }
    out.into()
}
