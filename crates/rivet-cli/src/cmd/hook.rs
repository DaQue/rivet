//! `hook run`: run a hook by name, the way the commands that fire it would.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::GlobalArgs;

/// Arguments to `rivet hook`.
#[derive(Debug, clap::Args)]
pub struct HookArgs {
    /// The subcommand (`run`).
    #[command(subcommand)]
    pub command: Option<HookCommand>,
}

/// The subcommands of `rivet hook`.
#[derive(Debug, clap::Subcommand)]
pub enum HookCommand {
    /// Run a hook.
    Run(HookRunArgs),
}

/// Arguments to `rivet hook run`.
#[derive(Debug, clap::Args)]
pub struct HookRunArgs {
    /// Allow a hook name Git does not know.
    #[arg(long = "allow-unknown-hook-name")]
    pub allow_unknown_hook_name: bool,

    /// Succeed silently when the hook does not exist.
    #[arg(long = "ignore-missing")]
    pub ignore_missing: bool,

    /// Feed this file to the hook's standard input.
    #[arg(long = "to-stdin", value_name = "PATH")]
    pub to_stdin: Option<PathBuf>,

    /// Accepted for compatibility; hooks always run one at a time.
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<i32>,

    /// The hook to run, then (after `--`) the arguments to pass to it.
    #[arg(value_name = "HOOK-NAME")]
    pub name: Option<String>,

    /// Arguments for the hook.
    #[arg(last = true, value_name = "HOOK-ARGS")]
    pub hook_args: Vec<String>,
}

const USAGE: &str = "usage: git hook run [--allow-unknown-hook-name] [--ignore-missing] [--to-stdin=<path>] [(-j|--jobs) <n>]
       <hook-name> [-- <hook-args>]

    --[no-]allow-unknown-hook-name
                          allow running a hook with a non-native hook name
    --[no-]ignore-missing silently ignore missing requested <hook-name>
    --[no-]to-stdin <path>
                          file to read into hooks' stdin
    -j, --[no-]jobs <n>   run up to <n> hooks simultaneously (-1 for CPU count)

";

/// Handler for `rivet hook`.
///
/// # Exit codes
///
/// The hook's own exit status; `1` when the hook does not exist (and `--ignore-missing` was not
/// given) or its name is unknown; `129` for a missing hook name or subcommand.
///
/// # Caveats
///
/// `hook list` and parallel execution are not implemented; `-j` is accepted and ignored.
pub fn run_hook(global: &GlobalArgs, args: &HookArgs) -> Result<i32> {
    let Some(HookCommand::Run(run)) = &args.command else {
        eprintln!("error: need a subcommand");
        eprintln!(
            "usage: git hook run [--allow-unknown-hook-name] [--ignore-missing] [--to-stdin=<path>] [(-j|--jobs) <n>]"
        );
        eprintln!("       <hook-name> [-- <hook-args>]");
        eprintln!(
            "   or: git hook list [--allow-unknown-hook-name] [-z] [--show-scope] <hook-name>"
        );
        eprintln!();
        return Ok(129);
    };
    let Some(name) = &run.name else {
        eprint!("{USAGE}");
        return Ok(129);
    };
    let repository = super::open(global)?;
    if !run.allow_unknown_hook_name && !rivet_core::hooks::NATIVE_HOOKS.contains(&name.as_str()) {
        eprintln!("error: unknown hook event '{name}';");
        eprintln!("use --allow-unknown-hook-name to allow non-native hook names");
        return Ok(1);
    }
    let mut hook = rivet_core::hooks::Hook::new(&repository, name);
    for arg in &run.hook_args {
        hook = hook.arg(arg);
    }
    if let Some(path) = &run.to_stdin {
        let data = std::fs::read(path).map_err(|e| {
            let text = e.to_string();
            let text = text.split(" (os error").next().unwrap_or(&text).to_string();
            anyhow::anyhow!("could not open '{}' for reading: {text}", path.display())
        })?;
        hook = hook.stdin(data);
    }
    match hook.run() {
        Some(code) => Ok(code),
        None if run.ignore_missing => Ok(0),
        None => {
            eprintln!("error: cannot find a hook named {name}");
            Ok(1)
        }
    }
}
