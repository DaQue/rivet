//! The `rivet` binary.
//!
//! A thin shell around `cmd::dispatch`: parse the command line, route it to a handler, and
//! turn whatever comes back into an exit code and a message on stderr. Everything that knows
//! about repositories, objects or refs lives in [`rivet_core`]; everything that knows about
//! this program's grammar lives in `cli`.
//!
//! # Layout
//!
//! * `cli` — the clap types: one `Args` struct per subcommand plus the global options
//!   (`--git-dir`, `--work-tree`, `-C`) that every handler receives.
//! * `cmd` — the handlers, one `run_*` function per subcommand, grouped by topic (`setup`,
//!   `snapshot`, `branching`, `history`, `remote`, `tag`, `ignore`, `plumbing`). Shared
//!   helpers — opening the repository, resolving a revision, printing a diffstat — live in
//!   `cmd` itself.
//!
//! No module here is part of a public API: the binary is the product, and `rivet-core` is
//! where reusable functionality is exposed.
//!
//! # The two error types
//!
//! `rivet-core` has exactly one error type, [`rivet_core::Error`], which is `thiserror`-derived
//! and never panics on repository data. The CLI wraps it in [`anyhow`] so that a handler can
//! attach the context that turns `NotARepository` into the sentence Git prints. **This binary
//! defines no error type of its own**, and `anyhow::Error` never crosses into `rivet-core`.
//!
//! # Exit codes
//!
//! `main` returns the code the handler chose; the constants are in `cmd`. The convention,
//! which every handler's doc comment states individually, is Git's:
//!
//! * `0` — success.
//! * `1` — the command ran and the answer is a documented negative (`diff --quiet` with
//!   differences, `check-ignore` with no match), or a partial failure a caller must look at
//!   (`push` with a rejected ref, `merge` with conflicts).
//! * `2` — a usage error inside a subcommand, which only `remote` returns.
//! * `128` — a fatal error: not a repository, an unknown revision, a corrupt object, a refused
//!   update.
//! * `129` — never returned by a handler; `clap` exits with it for an unrecognised command line,
//!   before `main` runs.

mod cli;
mod cmd;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

/// Git's `log -<N>` shorthand for `-n <N>`: clap cannot declare a numeric short flag, so the
/// argument vector is rewritten before parsing. Only `log` and `reflog` arguments before `--` are touched.
fn expand_count_shorthand(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    out.extend(iter.next()); // argv[0]
    let mut command_seen = false;
    let mut in_log = false;
    let mut after_dashes = false;
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().into_owned();
        if !command_seen {
            if text == "--repo" {
                out.push(arg);
                out.extend(iter.next());
                continue;
            }
            if !text.starts_with('-') {
                command_seen = true;
                in_log = text == "log" || text == "reflog";
            }
        } else if in_log && !after_dashes {
            if text == "--" {
                after_dashes = true;
            } else if text.len() > 1
                && text.starts_with('-')
                && text[1..].bytes().all(|b| b.is_ascii_digit())
            {
                out.push(format!("-n{}", &text[1..]).into());
                continue;
            }
        }
        out.push(arg);
    }
    out
}

fn main() -> ExitCode {
    let cli = Cli::parse_from(expand_count_shorthand(std::env::args_os().collect()));
    match cmd::dispatch(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            // Git's fatal diagnostics are one line on stderr, prefixed and unbounded; `{:#}`
            // keeps the whole anyhow chain on that line rather than printing a backtrace shape.
            eprintln!("fatal: {error:#}");
            ExitCode::from(cmd::EXIT_FATAL as u8)
        }
    }
}
