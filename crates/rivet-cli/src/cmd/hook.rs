//! `hook run` and `hook list`: run a hook by name, or list the hooks that would run for an event.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::GlobalArgs;

/// Arguments to `rivet hook`.
#[derive(Debug, clap::Args)]
pub struct HookArgs {
    /// The subcommand (`run`, `list`).
    #[command(subcommand)]
    pub command: Option<HookCommand>,
}

/// The subcommands of `rivet hook`.
#[derive(Debug, clap::Subcommand)]
pub enum HookCommand {
    /// Run a hook.
    Run(HookRunArgs),
    /// List the hooks that would run for an event.
    List(HookListArgs),
}

/// Arguments to `rivet hook list`.
#[derive(Debug, clap::Args)]
pub struct HookListArgs {
    /// Allow a hook name Git does not know.
    #[arg(long = "allow-unknown-hook-name")]
    pub allow_unknown_hook_name: bool,

    /// Terminate each line with NUL instead of a newline.
    #[arg(short = 'z')]
    pub nul: bool,

    /// Accepted for compatibility: only hooks from the hooks directory exist, and git shows no
    /// scope for those.
    #[arg(long = "show-scope")]
    pub show_scope: bool,

    /// The hook event to list.
    #[arg(value_name = "HOOK-NAME")]
    pub names: Vec<String>,
}

const LIST_USAGE: &str =
    "usage: git hook list [--allow-unknown-hook-name] [-z] [--show-scope] <hook-name>

    -z                    use NUL as line terminator
    --[no-]show-scope     show the config scope that defined each hook
    --[no-]allow-unknown-hook-name
                          allow running a hook with a non-native hook name

";

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
/// Parallel execution is not implemented; `-j` is accepted and ignored. `hook list` knows only the
/// hooks directory (`core.hooksPath` or `hooks/`); a repository that defines hooks in the
/// configuration (`hook.<name>.event` / `.command`) is refused by name rather than listed
/// incompletely.
pub fn run_hook(global: &GlobalArgs, args: &HookArgs) -> Result<i32> {
    if let Some(HookCommand::List(list)) = &args.command {
        return list_hooks(global, list);
    }
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

/// Whether any config file defines a hook through `hook.<name>.event` or `hook.<name>.command`.
fn config_defines_hooks(repository: &rivet_core::repo::Repository) -> bool {
    let defines = |config: &rivet_core::config::Config| {
        config.entries().iter().any(|e| {
            e.section.eq_ignore_ascii_case(b"hook")
                && e.subsection.is_some()
                && (e.key.eq_ignore_ascii_case(b"event") || e.key.eq_ignore_ascii_case(b"command"))
        })
    };
    defines(repository.config())
        || rivet_core::ops::load_outer_configs()
            .map(|outer| outer.iter().any(defines))
            .unwrap_or(false)
}

/// `hook list <hook-name>`: one line per hook that would run.
fn list_hooks(global: &GlobalArgs, list: &HookListArgs) -> Result<i32> {
    let [name] = list.names.as_slice() else {
        eprintln!("fatal: you must specify a hook event name to list");
        eprintln!();
        eprint!("{LIST_USAGE}");
        return Ok(129);
    };
    let repository = super::open(global)?;
    if !list.allow_unknown_hook_name && !rivet_core::hooks::NATIVE_HOOKS.contains(&name.as_str()) {
        eprintln!("error: unknown hook event '{name}';");
        eprintln!("use --allow-unknown-hook-name to allow non-native hook names");
        return Ok(1);
    }
    if config_defines_hooks(&repository) {
        eprintln!(
            "fatal: hooks defined in the configuration (hook.<name>.command) are not supported by rivet"
        );
        return Ok(128);
    }
    let hook = rivet_core::hooks::Hook::new(&repository, name);
    if hook.exists() {
        print!("hook from hookdir{}", if list.nul { '\0' } else { '\n' });
        return Ok(0);
    }
    // Nothing runs; this only lets the runner print its "not set as executable" hint.
    let _ = hook.run();
    eprintln!("warning: no hooks found for event '{name}'");
    Ok(1)
}
