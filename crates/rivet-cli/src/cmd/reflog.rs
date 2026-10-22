//! `rivet reflog [show] [<ref>]`: read a ref's reflog the way `git reflog show` does.

use anyhow::Result;
use bstr::BStr;
use rivet_core::RefName;

use crate::cli::GlobalArgs;

/// Arguments to `rivet reflog`.
///
/// # Caveats
///
/// * **`show` is optional and positional**: `reflog`, `reflog show` and `reflog show main` are
///   all accepted, as in Git. Only one ref may be named; Git's `expire`, `delete` and `exists`
///   subcommands are not implemented.
/// * **`--oneline` changes nothing**, because `git reflog` already prints one line per entry.
#[derive(Debug, clap::Args)]
pub struct ReflogArgs {
    /// `show` (the default), followed by the ref to read (default `HEAD`).
    #[arg(value_name = "REF")]
    pub args: Vec<String>,

    /// Stop after this many entries.
    #[arg(short = 'n', long = "max-count", value_name = "NUMBER")]
    pub max_count: Option<usize>,

    /// One line per entry (the default form).
    #[arg(long = "oneline")]
    pub oneline: bool,
}

/// Handler for `rivet reflog`.
///
/// # Exit codes
///
/// * `0` — the log was printed (an existing ref with no log prints nothing).
/// * `128` — the ref does not exist, or an unsupported subcommand was named.
pub fn run_reflog(global: &GlobalArgs, args: &ReflogArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let mut words = args.args.iter().map(String::as_str).peekable();
    if let Some(first) = words.peek().copied() {
        match first {
            "show" => {
                words.next();
            }
            "expire" | "delete" | "exists" => {
                anyhow::bail!("reflog {first} is not implemented");
            }
            _ => {}
        }
    }
    let spec = words.next().unwrap_or("HEAD");
    if words.next().is_some() {
        anyhow::bail!("reflog: only one ref may be named");
    }

    let name = if spec == "HEAD" {
        RefName::head()
    } else {
        repository
            .refs()
            .resolve_short(BStr::new(spec.as_bytes()))
            .map_err(|_| {
                anyhow::anyhow!(
                    "ambiguous argument '{spec}': unknown revision or path not in the working tree.\n\
                     Use '--' to separate paths from revisions, like this:\n\
                     'git <command> [<revision>...] -- [<file>...]'"
                )
            })?
    };

    let entries = repository.refs().reflog(&name)?;
    let mut out = Vec::new();
    for (position, entry) in entries
        .iter()
        .rev()
        .take(args.max_count.unwrap_or(usize::MAX))
        .enumerate()
    {
        out.extend_from_slice(
            format!(
                "{} {spec}@{{{position}}}: ",
                super::history::abbrev(&repository, &entry.new, false)
            )
            .as_bytes(),
        );
        out.extend_from_slice(&entry.message);
        out.push(b'\n');
    }
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&out).and_then(|_| stdout.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    Ok(0)
}
