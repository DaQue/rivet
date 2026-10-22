//! `format-patch`: write commits as e-mail style patches (`git am` can read them back).
//!
//! # What is implemented
//!
//! `format-patch [-N] [<since> | <a>..<b>] [-o <dir>] [--stdout] [--root] [-n]
//! [--start-number <n>] [--subject-prefix <p>] [--no-signature | --signature <s>]`. One file per
//! commit, `NNNN-<subject>.patch`, with the stat, the summary and a rename-aware diff. Merge
//! commits are skipped.
//!
//! # Caveats
//!
//! * **The signature line carries Rivet's version**, where Git's carries its own; scripts that
//!   compare patches byte for byte should pass `--no-signature`.
//! * Non-ASCII author names and subjects (which need RFC 2047 encoding), cover letters, threading,
//!   attachments and `-k` are not implemented and are refused.

use std::path::PathBuf;

use anyhow::Result;
use bstr::ByteSlice;
use rivet_core::revwalk::{SortOrder, WalkOptions};
use rivet_core::{ObjectId, ObjectKind};

use crate::cli::GlobalArgs;

/// Arguments of `format-patch`.
#[derive(Debug, clap::Args)]
pub struct FormatPatchArgs {
    /// `<since>`, or `<a>..<b>`.
    #[arg(value_name = "REV")]
    pub revs: Vec<String>,

    /// The number of commits to format (`-<N>`).
    #[arg(long = "max-count", value_name = "N")]
    pub max_count: Option<usize>,

    /// Write the files here instead of the current directory.
    #[arg(short = 'o', long = "output-directory", value_name = "DIR")]
    pub output_directory: Option<PathBuf>,

    /// Write all patches to standard output.
    #[arg(long = "stdout")]
    pub stdout: bool,

    /// Include the root commit as a patch against the empty tree.
    #[arg(long = "root")]
    pub root: bool,

    /// Number the patches `[PATCH n/m]` even if there is only one.
    #[arg(short = 'n', long = "numbered")]
    pub numbered: bool,

    /// Never number the patches.
    #[arg(long = "no-numbered")]
    pub no_numbered: bool,

    /// Start numbering at this number.
    #[arg(long = "start-number", value_name = "N", default_value_t = 1)]
    pub start_number: usize,

    /// Use this instead of `PATCH` in the subject.
    #[arg(
        long = "subject-prefix",
        value_name = "PREFIX",
        default_value = "PATCH"
    )]
    pub subject_prefix: String,

    /// Do not add a signature.
    #[arg(long = "no-signature")]
    pub no_signature: bool,

    /// Use this signature text.
    #[arg(long = "signature", value_name = "TEXT")]
    pub signature: Option<String>,

    /// Limit to commits touching these paths. Not implemented.
    #[arg(last = true, value_name = "PATH")]
    pub paths: Vec<String>,

    /// Keep the subject as it is. Not implemented.
    #[arg(short = 'k', long = "keep-subject")]
    pub keep_subject: bool,

    /// Generate a cover letter. Not implemented.
    #[arg(long = "cover-letter")]
    pub cover_letter: bool,
}

/// The `Subject:` header, folded at 78 columns with a one-space continuation indent. The
/// `[PATCH]` tag is not wrapped: when even the first word of the subject does not fit, the tag's
/// trailing space stays at the end of its line.
fn fold_subject(tag: &str, subject: &str) -> String {
    let mut out = format!("Subject: [{tag}] ");
    let mut line_len = out.len();
    for (index, word) in subject.split(' ').filter(|w| !w.is_empty()).enumerate() {
        if index == 0 {
            if line_len + word.len() > 78 {
                out.push_str("\n ");
                line_len = 1;
            }
        } else if line_len + 1 + word.len() > 78 {
            out.push_str("\n ");
            line_len = 1;
        } else {
            out.push(' ');
            line_len += 1;
        }
        out.push_str(word);
        line_len += word.len();
    }
    out.push('\n');
    out
}

/// Git's `format_sanitized_subject`, limited to the length Git gives a file name.
fn slug(subject: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    let mut chars = subject.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            if space {
                if !out.is_empty() {
                    out.push('-');
                }
                space = false;
            }
            out.push(c);
            if c == '.' {
                while chars.peek() == Some(&'.') {
                    chars.next();
                }
            }
        } else {
            space = true;
        }
    }
    while out.ends_with('.') || out.ends_with('-') {
        out.pop();
    }
    out.chars()
        .take(52)
        .collect::<String>()
        .trim_end_matches(['.', '-'])
        .to_string()
}

/// Handler for `rivet format-patch`.
///
/// # Exit codes
///
/// `0`; `128` for an unknown revision or an unsupported header.
pub fn run_format_patch(global: &GlobalArgs, args: &FormatPatchArgs) -> Result<i32> {
    if args.keep_subject || args.cover_letter || !args.paths.is_empty() {
        anyhow::bail!("format-patch: -k, --cover-letter and path limiting are not implemented");
    }
    let repository = super::open(global)?;
    let odb = repository.odb();

    let bad = |result: Result<ObjectId>, spec: &str| -> Result<ObjectId> {
        result.map_err(|_| {
            anyhow::anyhow!(
                "ambiguous argument '{spec}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
            )
        })
    };
    let mut options = WalkOptions::new();
    options.sort(SortOrder::Topo);
    let starts: Vec<ObjectId>;
    match (args.revs.as_slice(), args.max_count) {
        // Git would use the branch's upstream; with nothing configured it writes nothing.
        ([], None) => return Ok(0),
        ([], Some(_)) => {
            starts = vec![super::resolve_commit(&repository, "HEAD")?];
        }
        ([spec], Some(_)) if !spec.contains("..") => {
            starts = vec![bad(super::resolve_commit(&repository, spec), spec)?];
        }
        ([spec], None) if !spec.contains("..") => {
            if args.root {
                // `--root <rev>`: everything up to the revision.
                starts = vec![bad(super::resolve_commit(&repository, spec), spec)?];
            } else {
                options.exclude(bad(super::resolve_commit(&repository, spec), spec)?);
                starts = vec![super::resolve_commit(&repository, "HEAD")?];
            }
        }
        ([spec], max) if spec.contains("..") => {
            let (left, right) = spec.split_once("..").expect("checked");
            options.exclude(super::resolve_commit(
                &repository,
                if left.is_empty() { "HEAD" } else { left },
            )?);
            starts = vec![super::resolve_commit(
                &repository,
                if right.is_empty() { "HEAD" } else { right },
            )?];
            let _ = max;
        }
        _ => anyhow::bail!("format-patch: only a single revision or range is supported"),
    }
    // Newest first; `-N` counts patches (merges do not count), so widen the walk until it has
    // enough non-merge commits.
    let mut limit = args.max_count.unwrap_or(0);
    let mut selected: Vec<ObjectId>;
    loop {
        let mut walk = options.clone();
        if limit > 0 {
            walk.max_count(limit);
        }
        let commits = rivet_core::revwalk::walk_oids(odb, &starts, &walk)?;
        let walked = commits.len();
        selected = Vec::new();
        for oid in commits {
            let parents = repository.commit_parents(oid)?;
            if parents.len() > 1 || (parents.is_empty() && !args.root) {
                continue;
            }
            selected.push(oid);
        }
        match args.max_count {
            Some(n) if selected.len() < n && walked == limit => limit *= 2,
            _ => break,
        }
    }
    if let Some(n) = args.max_count {
        selected.truncate(n);
    }
    selected.reverse();

    let total = selected.len();
    let numbered = (total > 1 || args.numbered) && !args.no_numbered;
    let digits = (args.start_number + total.saturating_sub(1))
        .to_string()
        .len();
    let mut stdout_buffer: Vec<u8> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    if let Some(dir) = &args.output_directory {
        std::fs::create_dir_all(dir)?;
    }
    for (index, oid) in selected.iter().enumerate() {
        let number = args.start_number + index;
        let object = odb.read_typed(oid, ObjectKind::Commit)?;
        let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
        let parents = repository.commit_parents(*oid)?;

        // Subject: the first paragraph, joined.
        let message = commit.message;
        let mut paragraphs = message.splitn_str(2, "\n\n");
        let subject_raw = paragraphs.next().unwrap_or_default();
        let body_raw = paragraphs.next().unwrap_or_default();
        let subject = subject_raw
            .lines()
            .map(|l| String::from_utf8_lossy(l).trim().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        if !subject.is_ascii() || !commit.author.name.is_ascii() || !commit.author.email.is_ascii()
        {
            anyhow::bail!(
                "format-patch: non-ASCII headers (RFC 2047 encoding) are not implemented"
            );
        }

        let tag = if numbered {
            format!(
                "{} {number:0digits$}/{}",
                args.subject_prefix,
                args.start_number + total - 1
            )
        } else {
            args.subject_prefix.clone()
        };
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(format!("From {oid} Mon Sep 17 00:00:00 2001\n").as_bytes());
        out.extend_from_slice(
            format!("From: {} <{}>\n", commit.author.name, commit.author.email).as_bytes(),
        );
        let date = rivet_core::ops::format_date_style(
            "rfc2822",
            commit.author.when,
            commit.author.tz_offset_minutes,
            commit.author.when,
        )
        .unwrap_or_default();
        out.extend_from_slice(format!("Date: {date}\n").as_bytes());
        out.extend_from_slice(fold_subject(&tag, &subject).as_bytes());
        out.push(b'\n');
        let body = body_raw.trim_end();
        if !body.is_empty() {
            out.extend_from_slice(body);
            out.push(b'\n');
        }
        out.extend_from_slice(b"---\n");

        let base = match parents.first() {
            Some(parent) => repository.commit_tree(*parent)?,
            None => super::empty_tree(&repository)?,
        };
        let diff_options = rivet_core::DiffOptions::new();
        let mut diff =
            rivet_core::tree_diff(repository.format(), base, commit.tree, odb, &diff_options)?;
        super::detect_renames(&repository, &mut diff, &diff_options)?;
        rivet_core::diff::unified::format_stat_width(&diff, Some(odb), 72, &mut out)?;
        super::print_summary(&diff, &mut out);
        out.push(b'\n');
        rivet_core::format_unified(&diff, repository.format(), &mut out)?;
        if !args.no_signature {
            let signature = args
                .signature
                .clone()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
            out.extend_from_slice(format!("-- \n{signature}\n\n").as_bytes());
        }

        if args.stdout {
            // Without a signature Git still separates the patches on standard output.
            if args.no_signature && index > 0 {
                stdout_buffer.push(b'\n');
            }
            stdout_buffer.extend_from_slice(&out);
        } else {
            let file = format!(
                "{number:04}-{}.patch",
                slug(&String::from_utf8_lossy(
                    subject_raw.lines().next().unwrap_or_default()
                ))
            );
            let path = match &args.output_directory {
                Some(dir) => dir.join(&file),
                None => PathBuf::from(&file),
            };
            std::fs::write(&path, &out)?;
            names.push(path.display().to_string());
        }
    }
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    if args.stdout {
        stdout.write_all(&stdout_buffer)?;
    } else if !global.quiet {
        for name in names {
            writeln!(stdout, "{name}")?;
        }
    }
    Ok(0)
}
