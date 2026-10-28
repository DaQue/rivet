//! Attribute plumbing: `check-attr`.
//!
//! The attribute stack itself lives in `rivet_core::convert`; this command only parses git's
//! command line and prints one record per path and attribute.

use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::ignore::PathKind;

use crate::cli::GlobalArgs;

/// Arguments to `rivet check-attr`.
///
/// The words are kept raw because git decides what is an attribute and what is a path from the
/// position of a literal `--`, which a declarative parser would swallow.
#[derive(Debug, clap::Args)]
#[command(disable_help_flag = true)]
pub struct CheckAttrArgs {
    /// Options, attribute names and paths, as git's `check-attr` takes them.
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..
    )]
    pub args: Vec<std::ffi::OsString>,
}

const USAGE: &str =
    "usage: git check-attr [--source <tree-ish>] [-a | --all | <attr>...] [--] <pathname>...
   or: git check-attr --stdin [-z] [--source <tree-ish>] [-a | --all | <attr>...]

    -a, --[no-]all        report all attributes set on file
    --[no-]cached         use .gitattributes only from the index
    --[no-]stdin          read file names from stdin
    -z                    terminate input and output records by a NUL character
    --[no-]source <tree-ish>
                          which tree-ish to check attributes at

";

fn usage_error(message: &str) -> i32 {
    eprint!("error: {message}\n{USAGE}");
    129
}

/// Report the attributes of paths.
///
/// # Caveats
///
/// * **`-a` lists attributes in git's registration order, not file order.** Git numbers an
///   attribute the first time any file it reads mentions it, starting with `binary`, `diff`,
///   `merge`, `text`, and the table lives for the whole invocation, so the order can depend on
///   which paths were asked about first. Reading the global file, the root `.gitattributes`
///   and `info/attributes` before the sub-directory files is part of that order.
/// * **A path is a directory only when it ends in `/`.** Nothing is stat-ed, as in git.
/// * **`--cached` and `--source` are refused by name**, not ignored.
pub fn run_check_attr(global: &GlobalArgs, args: &CheckAttrArgs) -> Result<i32> {
    let mut all = false;
    let mut stdin = false;
    let mut nul = false;
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut options = true;
    let mut raw: Vec<Vec<u8>> = args.args.iter().map(|a| a.as_bytes().to_vec()).collect();
    // The command-line parser drops a `--` that directly follows the subcommand name.
    let mut argv = std::env::args_os()
        .skip_while(|a| a != "check-attr")
        .skip(1);
    if argv.next().is_some_and(|a| a == "--") {
        raw.insert(0, b"--".to_vec());
    }

    for word in raw {
        if !options || word == b"--" || !word.starts_with(b"-") || word == b"-" {
            if options && word == b"--" {
                options = false;
            }
            words.push(word);
            continue;
        }
        match &word[..] {
            b"--all" => all = true,
            b"--no-all" => all = false,
            b"--stdin" => stdin = true,
            b"--no-stdin" => stdin = false,
            b"--cached" | b"--no-cached" => {
                anyhow::bail!("check-attr: --cached is not supported by rivet")
            }
            w if w.starts_with(b"--source") => {
                anyhow::bail!("check-attr: --source is not supported by rivet")
            }
            w if !w.starts_with(b"--") => {
                for c in &w[1..] {
                    match c {
                        b'a' => all = true,
                        b'z' => nul = true,
                        _ => {
                            let shown = format!("unknown switch `{}'", *c as char);
                            return Ok(usage_error(&shown));
                        }
                    }
                }
            }
            w => {
                let shown = format!(
                    "unknown option `{}'",
                    String::from_utf8_lossy(&w[2..]).into_owned()
                );
                return Ok(usage_error(&shown));
            }
        }
    }

    // `words` still holds the literal `--` so its position can decide attributes from paths.
    let doubledash = words.iter().position(|w| w == b"--");
    let argc = words.len();
    let filei;
    let attr_end;
    if all {
        if doubledash.is_some_and(|d| d >= 1) {
            return Ok(usage_error("Attributes and --all both specified"));
        }
        filei = doubledash.map_or(0, |d| d + 1);
        attr_end = 0;
    } else if doubledash == Some(0) {
        return Ok(usage_error("No attribute specified"));
    } else if let Some(d) = doubledash {
        filei = d + 1;
        attr_end = d;
    } else {
        if argc == 0 {
            return Ok(usage_error("No attribute specified"));
        }
        filei = if stdin { argc } else { 1 };
        attr_end = filei;
    }
    let attrs: Vec<Vec<u8>> = words[..attr_end].to_vec();
    let mut paths: Vec<Vec<u8>> = words[filei.min(argc)..].to_vec();
    if stdin {
        if filei < argc {
            return Ok(usage_error("Can't specify files with --stdin"));
        }
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        let sep = if nul { 0u8 } else { b'\n' };
        let mut records: Vec<&[u8]> = input.split(|b| *b == sep).collect();
        if records.last().is_some_and(|r| r.is_empty()) {
            records.pop();
        }
        paths = records.into_iter().map(<[u8]>::to_vec).collect();
    } else if filei >= argc {
        return Ok(usage_error("No file specified"));
    }

    let repository = super::open(global)?;
    let worktree = repository.worktree()?;
    let Some(root) = worktree.root().map(Path::to_path_buf) else {
        anyhow::bail!("check-attr: this operation must be run in a work tree");
    };
    let cwd = std::env::current_dir()?;

    // git's attribute table lives for the whole run: names registered while answering an
    // earlier path still sit in front of those registered later.
    let mut table: Vec<BString> = Vec::new();
    let mut out = Vec::new();
    for shown in paths {
        let wants_dir = shown.ends_with(b"/");
        let given = Path::new(std::ffi::OsStr::from_bytes(&shown));
        let absolute = if given.is_absolute() {
            given.to_path_buf()
        } else {
            cwd.join(given)
        };
        let mut cleaned = std::path::PathBuf::new();
        for component in absolute.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    cleaned.pop();
                }
                other => cleaned.push(other.as_os_str()),
            }
        }
        let Ok(relative) = cleaned.strip_prefix(&root) else {
            eprintln!(
                "fatal: '{}' is outside repository at '{}'",
                String::from_utf8_lossy(&shown),
                root.display()
            );
            return Ok(128);
        };
        let relative = BString::from(relative.as_os_str().as_bytes());
        let stack = worktree.attr_stack(BStr::new(&relative[..]))?;
        for name in stack.registered() {
            if !table.contains(&name) {
                table.push(name);
            }
        }
        let kind = if wants_dir {
            PathKind::Directory
        } else {
            PathKind::File
        };
        let names: Vec<BString> = if all {
            table.clone()
        } else {
            attrs.iter().map(|a| BString::from(&a[..])).collect()
        };
        for name in names {
            let state = stack.lookup_kind(BStr::new(&relative[..]), kind, &name);
            if all && state == rivet_core::convert::Attr::Unspecified {
                continue;
            }
            let value = state.render();
            if nul {
                out.extend_from_slice(&shown);
                out.push(0);
                out.extend_from_slice(&name);
                out.push(0);
                out.extend_from_slice(&value);
                out.push(0);
            } else {
                out.extend_from_slice(&rivet_core::ops::quote_path(&shown, false));
                out.extend_from_slice(b": ");
                out.extend_from_slice(&name);
                out.extend_from_slice(b": ");
                out.extend_from_slice(&value);
                out.push(b'\n');
            }
        }
    }
    std::io::stdout().write_all(&out)?;
    Ok(0)
}
