//! The `rivet` binary.
//!
//! A thin shell around [`cmd::dispatch`]: parse the command line, route it to a handler, and
//! turn whatever comes back into an exit code and a message on stderr. Everything that knows
//! about repositories, objects or refs lives in `rivet-core`, and everything that knows about
//! this program's grammar lives in [`cli`].
//!
//! # Scaffold stage
//!
//! Every function in this binary is a doc comment and a `todo!()` body — the signatures and the
//! documentation are the design. See `docs/SCAFFOLD-BRIEF.md`.
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
//! `main` returns the code the handler chose; the constants are in [`cmd`]. The convention,
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

#![allow(dead_code)] // scaffold stage: everything is todo!() + unused
#![allow(unused_variables)] // scaffold stage: every parameter is unused until a body exists
#![allow(unused_imports)] // scaffold stage: imports are used by the todo!() bodies

mod cli;
mod cmd;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cmd::dispatch(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            // Git's fatal diagnostics are one line on stderr, prefixed and unbounded; `{:#}`
            // keeps the whole anyhow chain on that line rather than printing a backtrace shape.
            eprintln!("fatal: {error:#}");
            ExitCode::from(128)
        }
    }
}
