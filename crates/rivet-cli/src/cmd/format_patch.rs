//! `format-patch`: write commits as e-mail style patches (`git am` can read them back).
//!
//! # What is implemented
//!
//! `format-patch [-N] [<since> | <a>..<b>] [-o <dir>] [--stdout] [--root] [-n]
//! [--start-number <n>] [--subject-prefix <p>] [-k] [--no-signature | --signature <s>]`. One file per
//! commit, `NNNN-<subject>.patch`, with the stat, the summary and a rename-aware diff. Merge
//! commits are skipped.
//!
//! # Caveats
//!
//! * **The signature line carries Rivet's version**, where Git's carries its own; scripts that
//!   compare patches byte for byte should pass `--no-signature`.
//! * Attachments, path limiting and `-k` with a multi-line subject (which needs RFC 2047 encoding
//!   of an ASCII string) are not implemented and are refused.
//! * `--cover-letter` writes `0000-cover-letter.patch` (shortlog wrapped by bytes, then the diffstat
//!   of the whole range); `--thread[=shallow|deep]` and `--in-reply-to` add `Message-ID`,
//!   `In-Reply-To` and `References`, where the id carries the current time as git's does;
//!   non-ASCII names and subjects are encoded as RFC 2047 `q` words, folded at 76 columns, and
//!   a non-ASCII subject or message adds the `MIME-Version`/`Content-Type` headers.

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
    #[arg(long = "subject-prefix", value_name = "PREFIX")]
    pub subject_prefix: Option<String>,

    /// Do not add a signature.
    #[arg(long = "no-signature")]
    pub no_signature: bool,

    /// Use this signature text.
    #[arg(long = "signature", value_name = "TEXT")]
    pub signature: Option<String>,

    /// Limit to commits touching these paths. Not implemented.
    #[arg(last = true, value_name = "PATH")]
    pub paths: Vec<String>,

    /// Git has no such option; it is parsed only to fail with git's own message.
    #[arg(
        long = "directory",
        hide = true,
        num_args = 0..=1,
        require_equals = true,
        value_name = "DIR"
    )]
    pub directory: Option<Option<String>>,

    /// Keep the subject as it is: no `[PATCH]` tag.
    #[arg(short = 'k', long = "keep-subject")]
    pub keep_subject: bool,

    /// Generate a cover letter.
    #[arg(long = "cover-letter", overrides_with = "no_cover_letter")]
    pub cover_letter: bool,

    /// Do not generate a cover letter.
    #[arg(long = "no-cover-letter", overrides_with = "cover_letter")]
    pub no_cover_letter: bool,

    /// Thread the messages: `shallow` (the default) or `deep`.
    #[arg(
        long = "thread",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "shallow",
        overrides_with = "no_thread"
    )]
    pub thread: Option<String>,

    /// Do not thread the messages.
    #[arg(long = "no-thread", overrides_with = "thread")]
    pub no_thread: bool,

    /// Make the first mail (or all of them, with `--thread`) a reply to this message id.
    #[arg(long = "in-reply-to", value_name = "MESSAGE-ID")]
    pub in_reply_to: Option<String>,
}

/// How git's `add_rfc2047` decides what to escape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rfc2047 {
    /// A phrase (a name in `From:`): only letters, digits and `!*+-/` stay as they are.
    Phrase,
    /// A subject: everything printable stays but `=`, `?` and `_`.
    Subject,
}

/// The length of the last line of `buf`.
fn last_line_length(buf: &[u8]) -> usize {
    match buf.iter().rposition(|b| *b == b'\n') {
        Some(at) => buf.len() - at - 1,
        None => buf.len(),
    }
}

/// Whether text needs RFC 2047 at all: a non-ASCII byte (or ESC).
fn needs_rfc2047(text: &[u8]) -> bool {
    text.iter().any(|b| *b >= 0x80 || *b == 0x1b)
}

/// Append `text` as RFC 2047 `q` encoded words, one UTF-8 character at a time, folding with a
/// single leading blank so that no line passes 76 columns.
fn add_rfc2047(buf: &mut Vec<u8>, text: &[u8], kind: Rfc2047) {
    const ENCODING: &str = "UTF-8";
    let special = |byte: u8| -> bool {
        if matches!(byte, b'=' | b'?' | b'_') {
            return true;
        }
        match kind {
            Rfc2047::Subject => byte <= 32 || byte >= 127,
            Rfc2047::Phrase => !(byte.is_ascii_alphanumeric() || b"!*+-/".contains(&byte)),
        }
    };
    let mut line_len = last_line_length(buf);
    buf.extend_from_slice(format!("=?{ENCODING}?q?").as_bytes());
    line_len += ENCODING.len() + 5;
    let mut at = 0;
    while at < text.len() {
        let width = match text[at] {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        let end = (at + width).min(text.len());
        let chunk = &text[at..end];
        let encoded: usize = chunk.iter().map(|b| if special(*b) { 3 } else { 1 }).sum();
        if line_len + encoded + 2 > 76 {
            buf.extend_from_slice(format!("?=\n =?{ENCODING}?q?").as_bytes());
            line_len = ENCODING.len() + 5 + 1;
        }
        for byte in chunk {
            if special(*byte) {
                buf.extend_from_slice(format!("={byte:02X}").as_bytes());
            } else {
                buf.push(*byte);
            }
        }
        line_len += encoded;
        at = end;
    }
    buf.extend_from_slice(b"?=");
}

/// `From: <name> <email>` as git writes it: a non-ASCII name encoded, a name with an RFC 822
/// special quoted.
fn from_header(name: &[u8], email: &[u8]) -> Vec<u8> {
    let mut out = b"From: ".to_vec();
    if needs_rfc2047(name) {
        add_rfc2047(&mut out, name, Rfc2047::Phrase);
    } else if name.iter().any(|b| b"\"\\()<>[]:;@,.".contains(b)) {
        out.push(b'"');
        for byte in name {
            if matches!(byte, b'"' | b'\\') {
                out.push(b'\\');
            }
            out.push(*byte);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(name);
    }
    // The address goes on a line of its own when it would pass the 78 columns of RFC 2822.
    if last_line_length(&out) + 2 + email.len() + 1 > 78 {
        out.push(b'\n');
    }
    out.extend_from_slice(b" <");
    out.extend_from_slice(email);
    out.extend_from_slice(b">\n");
    out
}

/// A `Subject:` line with its `[PATCH n/m]` tag, the title encoded when it is not ASCII.
fn subject_header(tag: Option<&str>, title: &[u8]) -> Vec<u8> {
    if !needs_rfc2047(title) {
        return fold_subject(tag, &String::from_utf8_lossy(title)).into_bytes();
    }
    let mut out = match tag {
        Some(tag) => format!("Subject: [{tag}] ").into_bytes(),
        None => b"Subject: ".to_vec(),
    };
    add_rfc2047(&mut out, title, Rfc2047::Subject);
    out.push(b'\n');
    out
}

/// git's wrapped text for the shortlog: `text` indented by `first`, wrapped so that a line (indent
/// included) stays within `width` characters, the continuation lines indented by `rest`.
fn wrap_bytes(text: &[u8], first: usize, rest: usize, width: usize) -> Vec<u8> {
    let mut out = vec![b' '; first];
    let mut used = first;
    let mut start_of_line = true;
    for word in text.split(|b| *b == b' ').filter(|w| !w.is_empty()) {
        let columns = String::from_utf8_lossy(word).chars().count();
        if start_of_line {
            // the first word is never moved
        } else if used + 1 + columns <= width {
            out.push(b' ');
            used += 1;
        } else {
            out.push(b'\n');
            out.extend(std::iter::repeat_n(b' ', rest));
            used = rest;
        }
        out.extend_from_slice(word);
        used += columns;
        start_of_line = false;
    }
    out
}

/// The `MIME-Version` triple git adds when a subject or message is not ASCII.
const MIME_HEADERS: &[u8] =
    b"MIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\n";

/// The `Subject:` header, folded at 78 columns with a one-space continuation indent. The
/// `[PATCH]` tag is not wrapped: when even the first word of the subject does not fit, the tag's
/// trailing space stays at the end of its line. `None` (`-k`) writes the subject with no tag.
fn fold_subject(tag: Option<&str>, subject: &str) -> String {
    let mut out = match tag {
        Some(tag) => format!("Subject: [{tag}] "),
        None => "Subject: ".to_string(),
    };
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
    // The cut can leave a trailing `-` or `.`; git keeps it.
    out.chars().take(52).collect::<String>()
}

/// Handler for `rivet format-patch`.
///
/// # Exit codes
///
/// `0`; `128` for an unknown revision or an unsupported header.
pub fn run_format_patch(global: &GlobalArgs, args: &FormatPatchArgs) -> Result<i32> {
    if let Some(value) = &args.directory {
        match value {
            Some(value) => anyhow::bail!("unrecognized argument: --directory={value}"),
            None => anyhow::bail!("unrecognized argument: --directory"),
        }
    }
    if !args.paths.is_empty() {
        anyhow::bail!("format-patch: path limiting is not implemented");
    }
    if args.keep_subject {
        if args.subject_prefix.is_some() {
            anyhow::bail!("options '--subject-prefix/--rfc' and '-k' cannot be used together");
        }
        if args.numbered {
            anyhow::bail!("options '-n' and '-k' cannot be used together");
        }
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
            if parents.len() > 1 || (parents.is_empty() && !args.root && args.max_count.is_none()) {
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

    let cover = args.cover_letter && !args.no_cover_letter;
    let total = selected.len();
    let numbered = (total > 1 || args.numbered || cover) && !args.no_numbered;
    let last_number = args.start_number + total.saturating_sub(1);
    let digits = last_number.to_string().len();
    let thread: Option<bool> = if args.no_thread {
        None
    } else {
        match args.thread.as_deref() {
            None => None,
            Some("shallow" | "") => Some(false),
            Some("deep") => Some(true),
            // Git says nothing about a thread style it does not know and exits 129.
            Some(_) => return Ok(129),
        }
    };
    let in_reply_to = args.in_reply_to.as_ref().map(|id| {
        if id.starts_with('<') {
            id.clone()
        } else {
            format!("<{id}>")
        }
    });
    let identity = if (cover || thread.is_some()) && total > 0 {
        Some(repository.committer_signature()?)
    } else {
        None
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let message_id = |what: &str| -> String {
        let email = identity
            .as_ref()
            .map(|who| String::from_utf8_lossy(&who.email).into_owned())
            .unwrap_or_default();
        format!("<{what}.{now}.git.{email}>")
    };
    let mut ids: Vec<String> = Vec::new();
    // (file name, message, whether a patch follows another patch)
    let mut outputs: Vec<(String, Vec<u8>, bool)> = Vec::new();
    if let Some(dir) = &args.output_directory {
        std::fs::create_dir_all(dir)?;
    }
    let signature_text = |out: &mut Vec<u8>| {
        if !args.no_signature {
            let signature = args
                .signature
                .clone()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
            out.extend_from_slice(format!("-- \n{signature}\n\n").as_bytes());
        }
    };
    let subject_prefix = args.subject_prefix.as_deref().unwrap_or("PATCH");

    if cover && let (Some(who), Some(newest)) = (&identity, selected.last()) {
        let id = message_id("cover");
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(format!("From {newest} Mon Sep 17 00:00:00 2001\n").as_bytes());
        out.extend_from_slice(&thread_headers(thread, &in_reply_to, &ids, &id));
        out.extend_from_slice(&from_header(&who.name, &who.email));
        let date = rivet_core::ops::format_date_style(
            "rfc2822",
            who.when,
            who.tz_offset_minutes,
            who.when,
        )
        .unwrap_or_default();
        out.extend_from_slice(format!("Date: {date}\n").as_bytes());
        // The shortlog: authors by name, each with their subjects oldest first.
        let mut by_author: std::collections::BTreeMap<Vec<u8>, Vec<Vec<u8>>> =
            std::collections::BTreeMap::new();
        for oid in &selected {
            let object = odb.read_typed(oid, ObjectKind::Commit)?;
            let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
            let subject_raw = commit
                .message
                .splitn_str(2, "\n\n")
                .next()
                .unwrap_or_default();
            let subject: Vec<u8> = subject_raw
                .lines()
                .map(|line| line.trim().to_vec())
                .collect::<Vec<_>>()
                .join(&b' ');
            by_author
                .entry(commit.author.name.to_vec())
                .or_default()
                .push(subject);
        }
        let mut shortlog: Vec<u8> = Vec::new();
        for (name, subjects) in &by_author {
            shortlog.extend_from_slice(name);
            shortlog.extend_from_slice(format!(" ({}):\n", subjects.len()).as_bytes());
            for subject in subjects {
                shortlog.extend_from_slice(&wrap_bytes(subject, 2, 4, 72));
                shortlog.push(b'\n');
            }
            shortlog.push(b'\n');
        }
        let tag = if args.keep_subject {
            None
        } else if numbered {
            Some(format!("{subject_prefix} {:0digits$}/{last_number}", 0))
        } else {
            Some(subject_prefix.to_string())
        };
        out.extend_from_slice(&subject_header(tag.as_deref(), b"*** SUBJECT HERE ***"));
        if needs_rfc2047(&shortlog) {
            out.extend_from_slice(MIME_HEADERS);
        }
        out.extend_from_slice(b"\n*** BLURB HERE ***\n\n");
        out.extend_from_slice(&shortlog);
        let first = selected[0];
        // A range that starts at a root commit has no base to compare with: git prints no stat.
        if let Some(parent) = repository.commit_parents(first)?.first() {
            let base = repository.commit_tree(*parent)?;
            let head_tree = repository.commit_tree(*newest)?;
            let diff_options = rivet_core::DiffOptions::new();
            let mut diff =
                rivet_core::tree_diff(repository.format(), base, head_tree, odb, &diff_options)?;
            super::detect_renames(&repository, &mut diff, &diff_options)?;
            rivet_core::diff::unified::format_stat_width(&diff, Some(odb), 72, &mut out)?;
            super::print_summary(&diff, &mut out);
            out.push(b'\n');
        }
        signature_text(&mut out);
        ids.push(id);
        outputs.push(("0000-cover-letter.patch".to_string(), out, false));
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
        let subject_bytes: Vec<u8> = subject_raw
            .lines()
            .map(|line| line.trim().to_vec())
            .collect::<Vec<_>>()
            .join(&b' ');
        let subject = String::from_utf8_lossy(&subject_bytes).into_owned();
        // With `-k` the title is kept as written, line breaks included; only a non-ASCII one can
        // be sent that way (as encoded words).
        let raw_title = subject_raw.trim_end();
        let title: &[u8] = if args.keep_subject && needs_rfc2047(raw_title) {
            raw_title
        } else {
            &subject_bytes
        };

        let tag = if args.keep_subject {
            None
        } else if numbered {
            Some(format!("{subject_prefix} {number:0digits$}/{last_number}"))
        } else {
            Some(subject_prefix.to_string())
        };
        if args.keep_subject
            && !needs_rfc2047(title)
            && (subject_raw.trim_end().contains(&b'\n')
                || subject.contains("  ")
                || subject.contains('\t'))
        {
            anyhow::bail!(
                "format-patch: -k with a multi-line subject or runs of blanks (RFC 2047 encoding) is not implemented"
            );
        }
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(format!("From {oid} Mon Sep 17 00:00:00 2001\n").as_bytes());
        let id = message_id(&oid.to_string());
        out.extend_from_slice(&thread_headers(thread, &in_reply_to, &ids, &id));
        out.extend_from_slice(&from_header(&commit.author.name, &commit.author.email));
        let date = rivet_core::ops::format_date_style(
            "rfc2822",
            commit.author.when,
            commit.author.tz_offset_minutes,
            commit.author.when,
        )
        .unwrap_or_default();
        out.extend_from_slice(format!("Date: {date}\n").as_bytes());
        out.extend_from_slice(&subject_header(tag.as_deref(), title));
        if needs_rfc2047(title) || needs_rfc2047(body_raw) {
            out.extend_from_slice(MIME_HEADERS);
        }
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
        signature_text(&mut out);
        ids.push(id);
        let file = format!(
            "{number:04}-{}.patch",
            slug(&String::from_utf8_lossy(
                subject_raw.lines().next().unwrap_or_default()
            ))
        );
        outputs.push((file, out, index > 0));
    }

    let mut stdout_buffer: Vec<u8> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (file, out, after_patch) in outputs {
        if args.stdout {
            // Git puts a blank line before every patch after the first one on standard output,
            // signature or not.
            if after_patch {
                stdout_buffer.push(b'\n');
            }
            stdout_buffer.extend_from_slice(&out);
        } else {
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

/// `Message-ID`, `In-Reply-To` and `References` for message number `ids.len()` (the cover letter
/// counting as the first message), as git threads them: shallow mails all answer the first one (or
/// the id given with `--in-reply-to`), deep ones answer the one before.
fn thread_headers(
    thread: Option<bool>,
    in_reply_to: &Option<String>,
    ids: &[String],
    id: &str,
) -> Vec<u8> {
    let mut out = String::new();
    if thread.is_some() {
        out.push_str(&format!("Message-ID: {id}\n"));
    }
    let root = in_reply_to
        .clone()
        .or_else(|| thread.and(ids.first().cloned()));
    let deep = thread == Some(true) && !ids.is_empty();
    let reply = if deep {
        ids.last().cloned()
    } else {
        root.clone()
    };
    // A deep thread lists every earlier message (after the id given with `--in-reply-to`).
    let references: Vec<String> = if deep {
        in_reply_to.iter().chain(ids.iter()).cloned().collect()
    } else {
        root.into_iter().collect()
    };
    if let Some(reply) = reply {
        out.push_str(&format!(
            "In-Reply-To: {reply}\nReferences: {}\n",
            references.join("\n\t")
        ));
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_ascii_subject_folds_at_76_columns_between_whole_characters() {
        // The lines git wrote for this subject.
        let mut out = b"Subject: [PATCH 2/9] ".to_vec();
        add_rfc2047(
            &mut out,
            "Fünf Äpfel — test with a long subject that will need folding because it is rather long indeed yes".as_bytes(),
            Rfc2047::Subject,
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "Subject: [PATCH 2/9] =?UTF-8?q?F=C3=BCnf=20=C3=84pfel=20=E2=80=94=20test?=\n \
             =?UTF-8?q?=20with=20a=20long=20subject=20that=20will=20need=20folding=20b?=\n \
             =?UTF-8?q?ecause=20it=20is=20rather=20long=20indeed=20yes?="
        );
        assert!(text.lines().all(|line| line.len() <= 76));
    }

    #[test]
    fn names_escape_what_a_phrase_may_not_hold() {
        let mut out = b"From: ".to_vec();
        add_rfc2047(&mut out, "Dr. Ünter (x)".as_bytes(), Rfc2047::Phrase);
        assert_eq!(out, b"From: =?UTF-8?q?Dr=2E=20=C3=9Cnter=20=28x=29?=");
        assert_eq!(
            from_header(b"Dr. Who", b"w@x"),
            b"From: \"Dr. Who\" <w@x>\n"
        );
        assert_eq!(
            from_header(b"Plain Name", b"w@x"),
            b"From: Plain Name <w@x>\n"
        );
    }

    #[test]
    fn the_shortlog_wraps_at_72_columns_counting_characters() {
        let text =
            "leading ascii words then ünïcode in the middle and trailing ascii words follow here";
        let wrapped = wrap_bytes(text.as_bytes(), 2, 4, 72);
        assert_eq!(
            String::from_utf8(wrapped).unwrap(),
            "  leading ascii words then ünïcode in the middle and trailing ascii\n    words follow here"
        );
    }

    #[test]
    fn a_deep_thread_lists_every_earlier_message_in_references() {
        let ids: Vec<String> = ["<a>", "<b>", "<c>"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let headers = thread_headers(Some(true), &None, &ids, "<d>");
        assert_eq!(
            String::from_utf8(headers).unwrap(),
            "Message-ID: <d>\nIn-Reply-To: <c>\nReferences: <a>\n\t<b>\n\t<c>\n"
        );
        let shallow = thread_headers(Some(false), &None, &ids, "<d>");
        assert_eq!(
            String::from_utf8(shallow).unwrap(),
            "Message-ID: <d>\nIn-Reply-To: <a>\nReferences: <a>\n"
        );
        assert!(thread_headers(Some(false), &None, &[], "<a>") == b"Message-ID: <a>\n");
    }
}
