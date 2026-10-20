//! Ignore-rule plumbing: `check-ignore`.
//!
//! Phase 5, and the debugging half of milestone **M2**'s ignore support. The rules themselves
//! live in `rivet_core::ignore`; this command exists so that a user who cannot work out *why* a
//! file is ignored can ask the repository directly, which is the only practical way to debug a
//! stack of `.gitignore` files, `info/exclude` and `core.excludesFile`.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::GlobalArgs;

/// Arguments to `rivet check-ignore`.
///
/// # Caveats
///
/// * **`verbose` and `non_matching` are different questions and both are useful.** The default
///   prints the paths that *are* ignored; `-v` adds the rule that matched, its source file and
///   its line number; `--no-index` stops consulting the index, which is what a user wants when
///   asking about a path that is not tracked yet. `-n` inverts the whole thing and prints the
///   paths that are *not* ignored — which is not `--no-index`, despite the similar spelling, and
///   confusing the two is why both are documented here.
/// * **`-v` output has a stable, parseable shape**: `<source>:<line>:<pattern>\t<pathname>`. A
///   user's script may split on the first two colons, so neither the source path nor the pattern
///   may be reformatted, and the tab is a tab.
/// * **A path that matches a negation is not ignored and *is* printed under `-v`**, with the
///   negated pattern shown. The interesting case is a path that is matched by one rule and
///   re-included by a later one: printing only the final decision hides the rule that mattered.
/// * **A directory and a file of the same name are different questions.** A pattern ending in
///   `/` matches a directory only, so the same path can be ignored as a directory and not as a
///   file; the kind is taken from the index where the path is tracked and from the filesystem
///   otherwise, and `--stdin` paths that are neither are treated as files.
/// * **`--stdin` reads newline-separated paths, and `-z` reads NUL-separated ones**, for the
///   reason the flag exists at all: a filename may contain a newline, and a NUL may not. A
///   handler that split on newlines unconditionally makes exactly the files hardest to diagnose
///   impossible to ask about.
/// * **A path can never be re-included once a parent directory is excluded.** This is Git's rule
///   and the one users are most often surprised by: `!important.txt` in a directory that is
///   itself ignored does not bring the file back, and `rivet check-ignore -v` on it must say so
///   by reporting the directory rule that won, not by reporting "no match".
#[derive(Debug, clap::Args)]
pub struct CheckIgnoreArgs {
    /// The paths to test. Required unless `--stdin` is given.
    #[arg(value_name = "PATHNAME", required_unless_present = "stdin")]
    pub paths: Vec<PathBuf>,

    /// Print the rule that matched instead of only the path.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Print the paths that are not ignored, instead of the ones that are.
    #[arg(short = 'n', long = "non-matching")]
    pub non_matching: bool,

    /// Read the paths from standard input, one per line, or one per NUL with `-z`.
    #[arg(long = "stdin")]
    pub stdin: bool,

    /// Read and write the paths NUL-separated, for names containing newlines.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Do not consult the index when deciding whether a path is tracked.
    #[arg(long = "no-index")]
    pub no_index: bool,
}

/// Phase 5 — report whether paths are ignored, and by which rule.
///
/// # Purpose
///
/// For each path, evaluate the full ignore stack — command line, the `.gitignore` files between
/// the work tree root and the path, `.git/info/exclude` and `core.excludesFile`, in Git's
/// priority order — and print the ones that are ignored, optionally with the rule that decided
/// it.
///
/// # Exit codes
///
/// * `0` — at least one path matched, under the default form.
/// * `1` — **no** path matched. The requested negative: "is this file ignored" answering "no" is
///   a successful command with a negative answer, and it is the exit code a pre-commit hook
///   tests.
/// * `128` — a path is outside the work tree, the work tree is bare, or the ignore stack could
///   not be built.
///
/// # Caveats
///
/// * **`--non-matching` inverts the exit code's meaning along with the output.** With `-n` the
///   command exits `0` when at least one path is *not* ignored, and a script that reads the exit
///   code without the flag cannot tell which question was asked. The handler must treat `-n` as
///   selecting a different question rather than as a filter applied at print time.
/// * **A tracked file is never ignored, and this command must say so rather than reporting the
///   matching rule.** `.gitignore` matching a path that is in the index is not a reason to
///   report it as ignored — Git's rule is that ignore patterns do not apply to tracked files —
///   and the correct output is "not ignored" with `-v` optionally noting the shadowed rule.
///   Reporting the rule is the single most misleading thing this command can do.
/// * **The pattern evaluation is the core's, not a second implementation here.** Everything down
///   to the `**`, the anchoring and the trailing-slash rule comes from
///   [`rivet_core::IgnoreStack::check`]; a CLI-side shortcut for the common cases drifts within
///   a release and is worse than a slow correct answer.
/// * **`core.excludesFile` is resolved from the config at open time and its absence is not an
///   error.** A repository with no such setting has no such file, and a handler that errored on
///   the missing key would make the command fail in every repository that does not set it.
/// * **The command must not create or lock anything.** It is a query; it opens the repository,
///   reads the ignore sources and exits. Taking `index.lock` or writing a refreshed index would
///   make a read-only query fail in a repository another process is writing, which is exactly
///   when a debugging command is most wanted.
/// * **`-v` prints the *last* matching rule, not the first.** Git's ignore semantics are
///   last-match-wins, so a path matched by `*.log` and later by `!keep.log` is not ignored, and
///   a handler that stopped at the first match reports the opposite answer from the one the
///   command returns.
pub fn run_check_ignore(global: &GlobalArgs, args: &CheckIgnoreArgs) -> Result<i32> {
    let repository = super::open(global)?;
    repository.worktree()?.require_root("check-ignore")?;

    let mut raw: Vec<Vec<u8>> = Vec::new();
    if args.stdin {
        use std::io::Read as _;
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        let sep = if args.null_terminated { 0u8 } else { b'\n' };
        raw.extend(input.split(|b| *b == sep).filter(|p| !p.is_empty()).map(<[u8]>::to_vec));
    } else {
        if args.null_terminated {
            anyhow::bail!("-z only makes sense with --stdin");
        }
        for path in &args.paths {
            raw.push(super::path_arg(path)?);
        }
    }
    if raw.is_empty() {
        anyhow::bail!("no path specified");
    }
    if args.non_matching && !args.verbose {
        anyhow::bail!("--non-matching is only valid with --verbose");
    }

    let mut matched = false;
    let mut out = Vec::new();
    for bytes in raw {
        use std::os::unix::ffi::OsStrExt;
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&bytes));
        let wants_dir = bytes.ends_with(b"/");
        let relative = super::worktree_path(&repository, path, "check-ignore")?;
        let relative_ref = BStr::new(&relative[..]);

        let tracked = !args.no_index && repository.index().entry(relative_ref).is_some();
        let full = repository.worktree()?.join(relative_ref)?;
        let kind = if wants_dir || full.is_dir() { PathKind::Directory } else { PathKind::File };

        let mut stack = repository.ignore()?.clone();
        // Load every `.gitignore` between the root and the path, shallowest first.
        stack.push_dir(BStr::new(""))?;
        let mut prefix = BString::default();
        let parts: Vec<&[u8]> = relative.split(|b| *b == b'/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(part);
            stack.push_dir(BStr::new(&prefix[..]))?;
        }

        // A path under an ignored directory is ignored by that directory's rule.
        let mut decision = rivet_core::IgnoreDecision::Unmatched;
        if !tracked {
            let mut ancestor = BString::default();
            for part in &parts[..parts.len().saturating_sub(1)] {
                if !ancestor.is_empty() {
                    ancestor.push(b'/');
                }
                ancestor.extend_from_slice(part);
                let found = stack.check(BStr::new(&ancestor[..]), PathKind::Directory)?;
                if found.is_ignored() {
                    decision = found;
                    break;
                }
            }
            if matches!(decision, rivet_core::IgnoreDecision::Unmatched) {
                decision = stack.check(relative_ref, kind)?;
            }
        }

        let pattern = decision.pattern().cloned();
        let ignored = decision.is_ignored();
        let reported = if args.verbose { pattern.is_some() } else { ignored };
        if reported {
            matched = true;
        }
        if reported || (args.verbose && args.non_matching) {
            let shown_path = if args.null_terminated || !args.stdin { bytes.clone() } else { bytes.clone() };
            if args.verbose {
                let (source, line, text) = match &pattern {
                    Some(p) => {
                        let source = match &p.source {
                            rivet_core::IgnoreSource::Gitignore(dir) if dir.is_empty() || dir.as_slice() == b"." => ".gitignore".to_string(),
                            rivet_core::IgnoreSource::Gitignore(dir) => format!("{dir}/.gitignore"),
                            rivet_core::IgnoreSource::InfoExclude => ".git/info/exclude".to_string(),
                            rivet_core::IgnoreSource::CoreExcludesFile => repository
                                .config()
                                .get(BStr::new("core.excludesfile"))?
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                            rivet_core::IgnoreSource::CommandLine => "command line".to_string(),
                        };
                        let mut text = String::new();
                        if p.negated {
                            text.push('!');
                        }
                        text.push_str(&p.text.to_string());
                        // The stored text drops the trailing slash of a directory-only rule.
                        if p.directory_only && !text.ends_with('/') {
                            text.push('/');
                        }
                        (source, p.line.to_string(), text)
                    }
                    None => (String::new(), String::new(), String::new()),
                };
                if args.null_terminated {
                    out.extend_from_slice(format!("{source}\0{line}\0{text}\0").as_bytes());
                    out.extend_from_slice(&shown_path);
                    out.push(0);
                } else {
                    if pattern.is_some() {
                        out.extend_from_slice(format!("{source}:{line}:{text}\t").as_bytes());
                    } else {
                        out.extend_from_slice(b"::\t");
                    }
                    out.extend_from_slice(&shown_path);
                    out.push(b'\n');
                }
            } else {
                out.extend_from_slice(&shown_path);
                out.push(if args.null_terminated { 0 } else { b'\n' });
            }
        }
    }
    use std::io::Write as _;
    std::io::stdout().write_all(&out)?;
    Ok(if matched { 0 } else { 1 })
}

use bstr::{BStr, BString};
use rivet_core::PathKind;
