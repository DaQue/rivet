//! Commit rendering shared by `log` and `show`: the named layouts (`oneline`, `short`, `medium`,
//! `full`, `fuller`), user formats (`--format=%h %s`), and `--date=` styles.

use anyhow::Result;
use bstr::ByteSlice;
use rivet_core::{ObjectId, Signature};

/// How a commit is laid out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layout {
    Oneline,
    Short,
    Medium,
    Full,
    Fuller,
    /// A user format. `terminator` is `tformat:` semantics (a newline after every entry, what
    /// `--format=` means); without it entries are separated by a newline instead (`format:`).
    Format {
        template: String,
        terminator: bool,
    },
}

impl Layout {
    /// Parse a `--pretty=` / `--format=` value.
    pub fn parse(spec: &str) -> Result<Self> {
        Ok(match spec {
            "oneline" => Layout::Oneline,
            "short" => Layout::Short,
            "medium" => Layout::Medium,
            "full" => Layout::Full,
            "fuller" => Layout::Fuller,
            _ => {
                if let Some(rest) = spec.strip_prefix("format:") {
                    Layout::Format {
                        template: rest.to_string(),
                        terminator: false,
                    }
                } else if let Some(rest) = spec.strip_prefix("tformat:") {
                    Layout::Format {
                        template: rest.to_string(),
                        terminator: true,
                    }
                } else if spec.contains('%') {
                    Layout::Format {
                        template: spec.to_string(),
                        terminator: true,
                    }
                } else {
                    anyhow::bail!("invalid --pretty format: {spec}");
                }
            }
        })
    }

    /// Whether entries are blank-line separated multi-line blocks.
    pub fn is_block(&self) -> bool {
        matches!(
            self,
            Layout::Short | Layout::Medium | Layout::Full | Layout::Fuller
        )
    }

    /// Whether a diff following the entry is preceded by a blank line (everything but oneline).
    pub fn blank_before_diff(&self) -> bool {
        !matches!(self, Layout::Oneline)
    }
}

/// What a layout needs to know about one commit.
pub struct Entry<'a> {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: &'a [ObjectId],
    pub author: &'a Signature,
    pub committer: &'a Signature,
    pub message: &'a [u8],
    /// Ref names pointing at the commit, already in display order (`HEAD -> main`, `tag: v1`).
    pub decorations: &'a [String],
}

/// Rendering settings that do not depend on the commit.
pub struct Style<'a> {
    pub date: &'a str,
    pub now: i64,
    /// Abbreviate an id the way `--abbrev-commit` / `%h` do.
    pub abbrev: &'a dyn Fn(&ObjectId) -> String,
    /// Print full ids in `commit`, `Merge:` and `--oneline` lines.
    pub full_ids: bool,
}

impl Style<'_> {
    fn date_of(&self, sig: &Signature) -> String {
        rivet_core::ops::format_date_style(self.date, sig.when, sig.tz_offset_minutes, self.now)
            .unwrap_or_default()
    }
}

/// The first paragraph of a message joined by spaces (Git's subject).
pub fn subject(message: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for line in message.lines() {
        if line.iter().all(u8::is_ascii_whitespace) {
            if parts.is_empty() {
                continue;
            }
            break;
        }
        parts.push(line.trim());
    }
    parts.join(&b' ')
}

/// Everything after the subject paragraph and the blank lines that follow it.
pub fn body(message: &[u8]) -> &[u8] {
    let mut pos = 0;
    let mut in_subject = false;
    while pos < message.len() {
        let end = message[pos..]
            .find_byte(b'\n')
            .map_or(message.len(), |i| pos + i + 1);
        let blank = message[pos..end].iter().all(u8::is_ascii_whitespace);
        if !in_subject {
            if !blank {
                in_subject = true;
            }
        } else if blank {
            // Skip the blank separator lines.
            let mut rest = end;
            while rest < message.len() {
                let next = message[rest..]
                    .find_byte(b'\n')
                    .map_or(message.len(), |i| rest + i + 1);
                if !message[rest..next].iter().all(u8::is_ascii_whitespace) {
                    break;
                }
                rest = next;
            }
            return &message[rest..];
        }
        pos = end;
    }
    &[]
}

fn indent_lines(out: &mut Vec<u8>, text: &[u8]) {
    for line in text.lines() {
        out.extend_from_slice(b"    ");
        out.extend_from_slice(line);
        out.push(b'\n');
    }
}

fn person(sig: &Signature) -> String {
    format!("{} <{}>", sig.name, sig.email)
}

/// Render one commit. The result for a `Format` layout has no trailing newline; every other
/// layout ends with one.
pub fn render(layout: &Layout, e: &Entry<'_>, style: &Style<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    let id = |oid: &ObjectId| -> String {
        if style.full_ids {
            oid.to_string()
        } else {
            (style.abbrev)(oid)
        }
    };
    match layout {
        Layout::Oneline => {
            out.extend_from_slice(format!("{} ", id(&e.oid)).as_bytes());
            out.extend_from_slice(&subject(e.message));
            out.push(b'\n');
        }
        Layout::Format { template, .. } => expand(template, e, style, &mut out),
        _ => {
            let deco = if e.decorations.is_empty() {
                String::new()
            } else {
                format!(" ({})", e.decorations.join(", "))
            };
            out.extend_from_slice(format!("commit {}{deco}\n", e.oid).as_bytes());
            if e.parents.len() > 1 {
                let parents: Vec<String> = e.parents.iter().map(&id).collect();
                out.extend_from_slice(format!("Merge: {}\n", parents.join(" ")).as_bytes());
            }
            match layout {
                Layout::Full => {
                    out.extend_from_slice(format!("Author: {}\n", person(e.author)).as_bytes());
                    out.extend_from_slice(format!("Commit: {}\n", person(e.committer)).as_bytes());
                }
                Layout::Fuller => {
                    out.extend_from_slice(format!("Author:     {}\n", person(e.author)).as_bytes());
                    out.extend_from_slice(
                        format!("AuthorDate: {}\n", style.date_of(e.author)).as_bytes(),
                    );
                    out.extend_from_slice(
                        format!("Commit:     {}\n", person(e.committer)).as_bytes(),
                    );
                    out.extend_from_slice(
                        format!("CommitDate: {}\n", style.date_of(e.committer)).as_bytes(),
                    );
                }
                _ => {
                    out.extend_from_slice(format!("Author: {}\n", person(e.author)).as_bytes());
                    if *layout == Layout::Medium {
                        out.extend_from_slice(
                            format!("Date:   {}\n", style.date_of(e.author)).as_bytes(),
                        );
                    }
                }
            }
            out.push(b'\n');
            let message = e.message.trim_end_with(|c| c == '\n');
            if *layout == Layout::Short {
                let first: Vec<u8> = message
                    .lines()
                    .skip_while(|l| l.iter().all(u8::is_ascii_whitespace))
                    .take_while(|l| !l.iter().all(u8::is_ascii_whitespace))
                    .flat_map(|l| l.iter().copied().chain(std::iter::once(b'\n')))
                    .collect();
                indent_lines(&mut out, &first);
            } else {
                indent_lines(&mut out, message);
            }
        }
    }
    out
}

fn expand(template: &str, e: &Entry<'_>, style: &Style<'_>, out: &mut Vec<u8>) {
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest = &bytes[i + 1..];
        let (consumed, text) = match placeholder(rest, e, style) {
            Some(found) => found,
            None => {
                out.push(b'%');
                i += 1;
                continue;
            }
        };
        out.extend_from_slice(&text);
        i += 1 + consumed;
    }
}

fn placeholder(rest: &[u8], e: &Entry<'_>, style: &Style<'_>) -> Option<(usize, Vec<u8>)> {
    let sig_field = |sig: &Signature, field: u8| -> Option<Vec<u8>> {
        Some(match field {
            b'n' | b'N' => sig.name.to_vec(),
            b'e' | b'E' => sig.email.to_vec(),
            b'd' => style.date_of(sig).into_bytes(),
            b'D' => rivet_core::ops::format_date_style(
                "rfc2822",
                sig.when,
                sig.tz_offset_minutes,
                style.now,
            )?
            .into_bytes(),
            b'r' => rivet_core::ops::format_date_style(
                "relative",
                sig.when,
                sig.tz_offset_minutes,
                style.now,
            )?
            .into_bytes(),
            b'i' => rivet_core::ops::format_date_style(
                "iso",
                sig.when,
                sig.tz_offset_minutes,
                style.now,
            )?
            .into_bytes(),
            b'I' => rivet_core::ops::format_date_style(
                "iso-strict",
                sig.when,
                sig.tz_offset_minutes,
                style.now,
            )?
            .into_bytes(),
            b's' => rivet_core::ops::format_date_style(
                "short",
                sig.when,
                sig.tz_offset_minutes,
                style.now,
            )?
            .into_bytes(),
            b't' => sig.when.to_string().into_bytes(),
            _ => return None,
        })
    };
    if rest.len() >= 2 && (rest[0] == b'a' || rest[0] == b'c') && b"nNeEdDrIist".contains(&rest[1])
    {
        let sig = if rest[0] == b'a' {
            e.author
        } else {
            e.committer
        };
        return sig_field(sig, rest[1]).map(|t| (2, t));
    }
    if rest.len() >= 3 && rest[0] == b'x' {
        // %x00 – %x7f: a literal byte given in hex.
        let hex = std::str::from_utf8(&rest[1..3]).ok()?;
        return u8::from_str_radix(hex, 16).ok().map(|b| (3, vec![b]));
    }
    let first = *rest.first()?;
    let text: Vec<u8> = match first {
        b'H' => e.oid.to_string().into_bytes(),
        b'h' => (style.abbrev)(&e.oid).into_bytes(),
        b'T' => e.tree.to_string().into_bytes(),
        b't' => (style.abbrev)(&e.tree).into_bytes(),
        b'P' => e
            .parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
            .into_bytes(),
        b'p' => e
            .parents
            .iter()
            .map(style.abbrev)
            .collect::<Vec<_>>()
            .join(" ")
            .into_bytes(),
        b's' => subject(e.message),
        b'b' => body(e.message).to_vec(),
        b'B' => e.message.to_vec(),
        b'f' => sanitized(&subject(e.message)),
        b'D' => e.decorations.join(", ").into_bytes(),
        b'd' if e.decorations.is_empty() => Vec::new(),
        b'd' => format!(" ({})", e.decorations.join(", ")).into_bytes(),
        b'n' => b"\n".to_vec(),
        b'%' => b"%".to_vec(),
        _ => return None,
    };
    Some((1, text))
}

/// `%f`: the subject as a file name.
fn sanitized(subject: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pending_dash = false;
    for &b in subject {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' {
            if pending_dash && !out.is_empty() {
                out.push(b'-');
            }
            pending_dash = false;
            out.push(b);
        } else {
            pending_dash = true;
        }
    }
    while out.last() == Some(&b'.') {
        out.pop();
    }
    out
}

/// Seconds since the epoch, for `--date=relative`.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Validate a `--date=` value.
pub fn check_date_style(style: &str) -> Result<()> {
    if rivet_core::ops::is_date_style(style) {
        Ok(())
    } else {
        anyhow::bail!("unknown date format {style}")
    }
}
