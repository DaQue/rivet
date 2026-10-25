//! `symbolic-ref`: read, write and delete symbolic references.

use std::collections::HashSet;

use anyhow::{Result, bail};
use bstr::BStr;
use rivet_core::{RefName, RefTarget, ReflogMessage};

use crate::cli::GlobalArgs;

/// Arguments to `rivet symbolic-ref`.
#[derive(Debug, clap::Args)]
pub struct SymbolicRefArgs {
    /// Delete the symbolic ref.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Shorten the printed ref name where that is unambiguous.
    #[arg(long = "short", overrides_with = "no_short")]
    pub short: bool,

    /// Print the full ref name (the default).
    #[arg(long = "no-short", overrides_with = "short")]
    pub no_short: bool,

    /// Follow a chain of symbolic refs to its end (the default).
    #[arg(long = "recurse", overrides_with = "no_recurse")]
    pub recurse: bool,

    /// Print only the first hop of the chain.
    #[arg(long = "no-recurse", overrides_with = "recurse")]
    pub no_recurse: bool,

    /// The reason recorded in the reflog of a write.
    #[arg(short = 'm', value_name = "REASON")]
    pub message: Option<String>,

    /// `<name>` to read or delete, or `<name> <ref>` to write.
    #[arg(value_name = "NAME")]
    pub names: Vec<String>,
}

const USAGE: &str = "usage: git symbolic-ref [-m <reason>] <name> <ref>
   or: git symbolic-ref [-q] [--short] [--no-recurse] <name>
   or: git symbolic-ref --delete [-q] <name>

    -q, --[no-]quiet      suppress error message for non-symbolic (detached) refs
    -d, --[no-]delete     delete symbolic ref
    --[no-]short          shorten ref output
    --[no-]recurse        recursively dereference (default)
    -m <reason>           reason of the update

";

/// Whether Git accepts `name` as a ref name when one-level names are allowed.
///
/// `RefName` insists on a `/` for anything but an uppercase pseudo-ref, so a one-level name is
/// probed under a `refs/` prefix, where every other rule is the same.
fn git_valid(name: &str) -> bool {
    RefName::from_bytes(name.as_bytes()).is_ok()
        || RefName::from_bytes(format!("refs/{name}").as_bytes()).is_ok()
}

/// Handler for `rivet symbolic-ref`.
///
/// Reads (`<name>`), writes (`<name> <ref>`) or deletes (`--delete <name>`) a symbolic ref with
/// Git's messages.
///
/// # Exit codes
///
/// `0`; `1` for `-q` on a ref that is not symbolic, or a write to a malformed name; `128` for a
/// fatal error; `129` for the wrong number of arguments (the usage text goes to stderr).
///
/// # Caveats
///
/// * A one-level lowercase *target* (`symbolic-ref FOO foo`) is refused: [`RefName`] cannot hold
///   one, and Git accepts it.
pub fn run_symbolic_ref(global: &GlobalArgs, args: &SymbolicRefArgs) -> Result<i32> {
    let count_ok = if args.delete {
        args.names.len() == 1
    } else {
        matches!(args.names.len(), 1 | 2)
    };
    if !count_ok {
        eprint!("{USAGE}");
        return Ok(129);
    }
    let wanted = args.names.len();
    let repository = super::open(global)?;
    let name = args.names[0].as_str();

    if args.delete {
        if name == "HEAD" {
            bail!("deleting 'HEAD' is not allowed");
        }
        let parsed = RefName::from_bytes(name.as_bytes()).ok();
        let symbolic = parsed.as_ref().is_some_and(|p| {
            matches!(
                repository.refs().try_read(p),
                Ok(Some(RefTarget::Symbolic(_)))
            )
        });
        let Some(parsed) = parsed.filter(|_| symbolic) else {
            bail!("Cannot delete {name}, not a symbolic ref");
        };
        repository.refs().delete(&parsed)?;
        return Ok(0);
    }

    if wanted == 2 {
        let target = args.names[1].as_str();
        if name == "HEAD" && !target.starts_with("refs/") {
            bail!("Refusing to point HEAD outside of refs/");
        }
        if !git_valid(target) {
            bail!("Refusing to set '{name}' to invalid ref '{target}'");
        }
        let Ok(parsed) = RefName::from_bytes(name.as_bytes()) else {
            eprintln!("error: refusing to update ref with bad name '{name}'");
            return Ok(1);
        };
        let Ok(target_ref) = RefName::from_bytes(target.as_bytes()) else {
            bail!("not implemented: symbolic-ref to the one-level target '{target}'");
        };
        // Git logs the update (empty message unless `-m`) when the new target resolves to an
        // object, and only for refs that keep a reflog.
        let who = repository.committer_signature()?;
        let reason = args.message.clone().unwrap_or_default();
        let resolves = repository.refs().resolve_oid(&target_ref).is_ok();
        let message = (resolves && repository.should_log_ref(&parsed))
            .then(|| ReflogMessage::new(&who, BStr::new(reason.as_bytes())));
        repository
            .refs()
            .set_symbolic(&parsed, &target_ref, message)?;
        return Ok(0);
    }

    // Read.
    if !git_valid(name) {
        bail!("No such ref: {name}");
    }
    let mut current = RefName::from_bytes(name.as_bytes()).ok();
    let mut last = None;
    let mut hops = 0;
    while let Some(ref_name) = current.take() {
        match repository.refs().try_read(&ref_name)? {
            Some(RefTarget::Symbolic(next)) => {
                hops += 1;
                if hops > 5 + 5 {
                    bail!("No such ref: {name}");
                }
                last = Some(next.clone());
                if args.no_recurse {
                    break;
                }
                current = Some(next);
            }
            _ => break,
        }
    }
    let Some(result) = last else {
        if global.quiet {
            return Ok(1);
        }
        bail!("ref {name} is not a symbolic ref");
    };
    let full = result.to_string();
    if args.short {
        let names: Vec<String> = repository
            .refs()
            .list(b"refs/")?
            .into_iter()
            .map(|(n, _)| n.to_string())
            .collect();
        let all: HashSet<&str> = names.iter().map(String::as_str).collect();
        println!("{}", super::query::shorten(&full, &all));
    } else {
        println!("{full}");
    }
    Ok(0)
}
