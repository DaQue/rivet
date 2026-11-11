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

/// `-M[<n>]` or `-C[<n>]`: the rename switches `log` shares with `diff`.
fn is_rename_short(text: &str) -> bool {
    text.starts_with("-M") || text.starts_with("-C")
}

/// Git's `log -<N>` shorthand for `-n <N>`: clap cannot declare a numeric short flag, so the
/// argument vector is rewritten before parsing. Only `log` and `reflog` arguments before `--` are touched; for `format-patch` the same shorthand becomes `--max-count=<N>`.
fn expand_count_shorthand(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    out.extend(iter.next()); // argv[0]
    let mut command_seen = false;
    let mut in_log = false;
    let mut in_format_patch = false;
    let mut in_blame = false;
    let mut in_config = false;
    let mut in_diff = false;
    let mut in_show = false;
    let mut after_dashes = false;
    while let Some(mut arg) = iter.next() {
        let mut text = arg.to_string_lossy().into_owned();
        if in_config && command_seen && !after_dashes && text.starts_with("-t") {
            // git's `-t <type>` / `-t<type>` is `--type`; clap's config has no such short flag.
            let spelled = match &text[2..] {
                "" => "--type".to_owned(),
                value => format!("--type={value}"),
            };
            arg = spelled.clone().into();
            text = spelled;
        }
        if !command_seen {
            if text == "--repo" {
                out.push(arg);
                out.extend(iter.next());
                continue;
            }
            if !text.starts_with('-') {
                command_seen = true;
                in_log = text == "log" || text == "reflog";
                in_format_patch = text == "format-patch";
                in_blame = text == "blame";
                in_config = text == "config";
                in_diff = text == "diff";
                in_show = text == "show";
            }
        } else if in_format_patch && !after_dashes {
            if text == "--" {
                after_dashes = true;
            } else if text.len() > 1
                && text.starts_with('-')
                && text[1..].bytes().all(|b| b.is_ascii_digit())
            {
                out.push(format!("--max-count={}", &text[1..]).into());
                continue;
            } else if let Some(count) = text.strip_prefix("-v") {
                // `-v<n>` / `-v <n>` is `--reroll-count`; `-v` is already the global `--verbose`.
                out.push(if count.is_empty() {
                    "--reroll-count".into()
                } else {
                    format!("--reroll-count={count}").into()
                });
                continue;
            }
        } else if in_blame && !after_dashes {
            // `-M[<score>]` and `-C[<score>]` take an optional attached number; clap would let
            // them swallow the next argument, so they become long options.
            if text == "--" {
                after_dashes = true;
            } else if let Some(score) = text.strip_prefix("-M")
                && score.bytes().all(|b| b.is_ascii_digit())
            {
                out.push("--detect-moves".into());
                if !score.is_empty() {
                    out.push(format!("--move-score={score}").into());
                }
                continue;
            } else if let Some(score) = text.strip_prefix("-C")
                && score.bytes().all(|b| b.is_ascii_digit())
            {
                out.push("--detect-copies".into());
                if !score.is_empty() {
                    out.push(format!("--copy-score={score}").into());
                }
                continue;
            }
        } else if in_config && !after_dashes && (text == "--type" || text.starts_with("--type=")) {
            // git checks the name while it parses options, so a name it does not know is fatal
            // whatever else is on the line; clap's own error would read differently.
            let value = match text.strip_prefix("--type=") {
                Some(value) => Some(value.to_owned()),
                None => iter.next().map(|next| {
                    let value = next.to_string_lossy().into_owned();
                    out.push(arg.clone());
                    out.push(next);
                    value
                }),
            };
            if let Some(value) = value {
                if !matches!(
                    value.as_str(),
                    "bool"
                        | "int"
                        | "bool-or-int"
                        | "bool-or-str"
                        | "path"
                        | "expiry-date"
                        | "color"
                ) {
                    eprintln!("fatal: unrecognized --type argument, {value}");
                    std::process::exit(cmd::EXIT_FATAL);
                }
                if text == "--type" {
                    continue;
                }
            }
        } else if in_config && text == "--get-regexp" {
            // git's `--get-regexp` is a mode flag, so `-z` may follow it; clap's takes the
            // next word as its pattern. Plain flags right after it move in front, so the
            // pattern is the first word that is none of them (git's pattern follows any options).
            while let Some(next) = iter.as_slice().first().filter(|next| match next.to_str() {
                Some("-z" | "--null" | "--global" | "--local" | "--system") => true,
                Some("--type" | "-t" | "--file" | "-f") => iter.as_slice().len() > 1,
                Some(word) => {
                    word.starts_with("--type=")
                        || word.starts_with("--file=")
                        || (word.starts_with("-t") && word.len() > 2)
                }
                None => false,
            }) {
                let next = next.clone();
                iter.next();
                let spelled = next.to_string_lossy().into_owned();
                // `-t` is `--type`; `-f` / `--file <path>` take their path from the next word.
                let spelled = match spelled.as_str() {
                    "-t" => "--type".to_owned(),
                    "-f" => "--file".to_owned(),
                    word if word.starts_with("-t") && !word.starts_with("--") => {
                        format!("--type={}", &word[2..])
                    }
                    _ => spelled,
                };
                let two_word = spelled == "--type" || spelled == "--file";
                let value = if spelled == "--type" {
                    iter.as_slice()
                        .first()
                        .map(|v| v.to_string_lossy().into_owned())
                } else {
                    spelled.strip_prefix("--type=").map(str::to_owned)
                };
                if let Some(value) = value
                    && !matches!(
                        value.as_str(),
                        "bool"
                            | "int"
                            | "bool-or-int"
                            | "bool-or-str"
                            | "path"
                            | "expiry-date"
                            | "color"
                    )
                {
                    eprintln!("fatal: unrecognized --type argument, {value}");
                    std::process::exit(cmd::EXIT_FATAL);
                }
                out.push(spelled.into());
                if two_word && let Some(value) = iter.next() {
                    out.push(value);
                }
            }
            // git stops option parsing at the first non-option word: a `--` right before the
            // pattern is eaten, and every word after the pattern is a plain word (the value
            // pattern, or one too many), even when it looks like an option.
            if iter.as_slice().first().is_some_and(|next| next == "--") {
                iter.next();
            }
            match iter.next() {
                Some(pattern) => {
                    let mut joined = std::ffi::OsString::from("--get-regexp=");
                    joined.push(pattern);
                    out.push(joined);
                    let rest: Vec<_> = iter.by_ref().collect();
                    if !rest.is_empty() {
                        out.push("--".into());
                        out.extend(rest);
                    }
                }
                None => out.push(arg),
            }
            continue;
        } else if (in_diff || in_show || (in_log && text.len() > 1 && is_rename_short(&text)))
            && !after_dashes
        {
            // `-M<n>` is `--find-renames=<n>`: clap cannot take an attached optional value.
            if text == "--" {
                after_dashes = true;
            } else if let Some(score) = text.strip_prefix("-M") {
                out.push(format!("--find-renames={score}").into());
                continue;
            } else if let Some(score) = text.strip_prefix("-C") {
                out.push(format!("--find-copies={score}").into());
                continue;
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
    let args = expand_count_shorthand(std::env::args_os().collect());
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            // `git log` (and so `bisect visualize`) reports an option it does not know as one
            // fatal line, not as a usage screen.
            use clap::error::{ContextKind, ContextValue, ErrorKind};
            if error.kind() == ErrorKind::UnknownArgument
                && matches!(error.get(ContextKind::Usage), Some(ContextValue::StyledStr(usage))
                    if usage.to_string().starts_with("Usage: rivet log "))
                && let Some(ContextValue::String(arg)) = error.get(ContextKind::InvalidArg)
            {
                // Git names the word as typed: `--bogus=3`, or a bundle like `-xy`.
                let typed = args
                    .iter()
                    .filter_map(|a| a.to_str())
                    .find(|a| a.starts_with(arg.as_str()))
                    .unwrap_or(arg);
                eprintln!("fatal: unrecognized argument: {typed}");
                return ExitCode::from(cmd::EXIT_FATAL as u8);
            }
            error.exit()
        }
    };
    match cmd::dispatch(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            // A refused or missing credential reads like Git's, not like the library's error.
            if let Some(rivet_core::Error::Authentication { url, reason }) =
                error.downcast_ref::<rivet_core::Error>()
            {
                if reason.starts_with("RPC failed; ") {
                    eprintln!("error: {reason}");
                } else if reason == "unable to access" {
                    let url = url.to_string();
                    let slash = if url.ends_with('/') { "" } else { "/" };
                    eprintln!(
                        "fatal: unable to access '{url}{slash}': The requested URL returned error: 401"
                    );
                } else if reason.starts_with("could not read ")
                    || reason.starts_with("credential helper ")
                    || reason.starts_with("credential url ")
                {
                    eprintln!("fatal: {reason}");
                } else {
                    let url = url.to_string();
                    let slash = if url.ends_with('/') { "" } else { "/" };
                    eprintln!("fatal: Authentication failed for '{url}{slash}'");
                }
                return ExitCode::from(cmd::EXIT_FATAL as u8);
            }
            // Git's fatal diagnostics are one line on stderr, prefixed and unbounded; `{:#}`
            // keeps the whole anyhow chain on that line rather than printing a backtrace shape.
            eprintln!("fatal: {error:#}");
            ExitCode::from(cmd::EXIT_FATAL as u8)
        }
    }
}
