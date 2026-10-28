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
                } else if spec.is_empty() || spec.contains('%') {
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
        match self {
            Layout::Oneline => false,
            Layout::Format { template, .. } => !template.is_empty(),
            _ => true,
        }
    }
}

/// Which colour a piece of a decoration is painted with under `--color`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shade {
    /// The separators and the ` -> ` arrow: the colour of the `commit` line.
    Commit,
    Head,
    Branch,
    Remote,
    Tag,
    Stash,
}

/// One ref name decorating a commit, as the pieces it is painted in (`HEAD`, ` -> `, `main`).
#[derive(Clone, Debug)]
pub struct Decoration {
    pub pieces: Vec<(Shade, String)>,
}

impl Decoration {
    /// The label without colour (`HEAD -> main`).
    pub fn plain(&self) -> String {
        self.pieces.iter().map(|(_, text)| text.as_str()).collect()
    }
}

fn plain_labels(decorations: &[Decoration]) -> Vec<String> {
    decorations.iter().map(Decoration::plain).collect()
}

/// The escape sequences `log --color` paints with; an empty one paints nothing but still resets.
#[derive(Clone, Debug)]
pub struct Palette {
    pub commit: String,
    pub head: String,
    pub branch: String,
    pub remote: String,
    pub tag: String,
    pub stash: String,
}

impl Palette {
    /// Git's defaults: `commit` yellow, and bold cyan / green / red / yellow / magenta for `HEAD`,
    /// local branches, remote branches, tags and the stash.
    pub fn git_default() -> Self {
        Palette {
            commit: "\x1b[33m".to_string(),
            head: "\x1b[1;36m".to_string(),
            branch: "\x1b[1;32m".to_string(),
            remote: "\x1b[1;31m".to_string(),
            tag: "\x1b[1;33m".to_string(),
            stash: "\x1b[1;35m".to_string(),
        }
    }

    fn of(&self, shade: Shade) -> &str {
        match shade {
            Shade::Commit => &self.commit,
            Shade::Head => &self.head,
            Shade::Branch => &self.branch,
            Shade::Remote => &self.remote,
            Shade::Tag => &self.tag,
            Shade::Stash => &self.stash,
        }
    }

    fn paint(&self, out: &mut Vec<u8>, shade: Shade, text: &[u8]) {
        out.extend_from_slice(self.of(shade).as_bytes());
        out.extend_from_slice(text);
        out.extend_from_slice(b"\x1b[m");
    }
}

/// The escape sequence for a git colour specification (`bold red`, `#ff8000`, `200`), or `None`
/// for one this does not understand. An empty or `normal` specification gives an empty sequence.
pub fn parse_color_spec(spec: &str) -> Option<String> {
    const ATTRIBUTES: [(&str, u32); 7] = [
        ("bold", 1),
        ("dim", 2),
        ("italic", 3),
        ("ul", 4),
        ("blink", 5),
        ("reverse", 7),
        ("strike", 9),
    ];
    let mut attributes: Vec<u32> = Vec::new();
    let mut codes: Vec<String> = Vec::new();
    let mut colors = 0;
    let mut words = spec.split_whitespace().peekable();
    if words.peek() == Some(&"reset") {
        words.next();
        return words.next().is_none().then(|| "\x1b[m".to_string());
    }
    for word in words {
        if let Some((_, code)) = ATTRIBUTES.iter().find(|(name, _)| *name == word) {
            attributes.push(*code);
            continue;
        }
        let fg = colors == 0;
        let named = [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ];
        let code = if word == "normal" {
            None
        } else if let Some(n) = named.iter().position(|name| *name == word) {
            Some(format!("{}", if fg { 30 } else { 40 } + n))
        } else if let Some(n) = word
            .strip_prefix("bright")
            .and_then(|rest| named.iter().position(|name| *name == rest))
        {
            Some(format!("{}", if fg { 90 } else { 100 } + n))
        } else if let Ok(n) = word.parse::<u32>() {
            match n {
                0..=7 => Some(format!("{}", if fg { 30 } else { 40 } + n)),
                8..=15 => Some(format!("{}", if fg { 90 } else { 100 } + n - 8)),
                16..=255 => Some(format!("{};5;{n}", if fg { 38 } else { 48 })),
                _ => return None,
            }
        } else if word.starts_with('#') {
            let hex = word.strip_prefix('#').filter(|h| h.len() == 6)?;
            let value = u32::from_str_radix(hex, 16).ok()?;
            Some(format!(
                "{};2;{};{};{}",
                if fg { 38 } else { 48 },
                value >> 16,
                (value >> 8) & 0xff,
                value & 0xff
            ))
        } else {
            return None;
        };
        colors += 1;
        if colors > 2 {
            return None;
        }
        if let Some(code) = code {
            codes.push(code);
        }
    }
    attributes.sort_unstable();
    attributes.dedup();
    let mut all: Vec<String> = attributes.iter().map(u32::to_string).collect();
    all.extend(codes);
    Some(if all.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", all.join(";"))
    })
}

/// Which parts of an identity line git's `split_ident_line` found. [`Signature`] fills the parts a
/// malformed line lacks with empty values, so the renderer asks this before printing them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presence {
    /// A `<email>` was found: without it git leaves out the `Author:`/`Commit:` line.
    pub mail: bool,
    /// The decimal timestamp exactly as written, present only when a `+`/`-` zone follows it.
    pub date: Option<Vec<u8>>,
}

impl Presence {
    /// Read the identity on the `header` line (`author`, `committer`) of a raw commit body.
    pub fn of_header(raw: &[u8], header: &[u8]) -> Self {
        for line in raw.lines() {
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix(header).and_then(|v| v.strip_prefix(b" ")) {
                return Self::of_ident(value);
            }
        }
        Self::default()
    }

    fn of_ident(line: &[u8]) -> Self {
        let Some(open) = line.find_byte(b'<') else {
            return Self::default();
        };
        let Some(close) = line[open + 1..].find_byte(b'>') else {
            return Self::default();
        };
        let rest = line[open + 1 + close + 1..].trim_start_with(|c| c == ' ');
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let after = rest[digits..].trim_start_with(|c| c == ' ');
        let zone = matches!(after.first(), Some(b'+' | b'-'))
            && after[1..].first().is_some_and(u8::is_ascii_digit);
        Self {
            mail: true,
            date: (digits > 0 && zone).then(|| rest[..digits].to_vec()),
        }
    }
}

/// What a layout needs to know about one commit.
pub struct Entry<'a> {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: &'a [ObjectId],
    pub author: &'a Signature,
    pub committer: &'a Signature,
    pub author_presence: &'a Presence,
    pub committer_presence: &'a Presence,
    pub message: &'a [u8],
    /// Ref names pointing at the commit, already in display order (`HEAD -> main`, `tag: v1`).
    pub decorations: &'a [Decoration],
}

/// Rendering settings that do not depend on the commit.
pub struct Style<'a> {
    pub date: &'a str,
    pub now: i64,
    /// Abbreviate an id the way `--abbrev-commit` / `%h` do.
    pub abbrev: &'a dyn Fn(&ObjectId) -> String,
    /// Print full ids in `commit`, `Merge:` and `--oneline` lines.
    pub full_ids: bool,
    /// Abbreviate the id on the `commit` line of the block layouts (`--oneline` leaves this on).
    pub abbrev_commit: bool,
    /// Paint the `commit` line (or the oneline id) and its decorations; `None` prints plain text.
    pub palette: Option<&'a Palette>,
}

impl Style<'_> {
    /// The date text; an identity with no usable date (git: no `+zone` after the digits) is the epoch.
    fn date_of(&self, sig: &Signature, presence: &Presence) -> String {
        let (when, tz) = if presence.date.is_some() {
            (sig.when, sig.tz_offset_minutes)
        } else {
            (0, 0)
        };
        rivet_core::ops::format_date_style(self.date, when, tz, self.now).unwrap_or_default()
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
            push_commit_id(&mut out, id(&e.oid).as_bytes(), style);
            push_decorations(&mut out, e.decorations, style);
            out.push(b' ');
            out.extend_from_slice(&subject(e.message));
            out.push(b'\n');
        }
        Layout::Format { template, .. } => expand(template, e, style, &mut out),
        _ => {
            let head = if style.abbrev_commit && !style.full_ids {
                (style.abbrev)(&e.oid)
            } else {
                e.oid.to_string()
            };
            push_commit_id(&mut out, format!("commit {head}").as_bytes(), style);
            push_decorations(&mut out, e.decorations, style);
            out.push(b'\n');
            if e.parents.len() > 1 {
                let parents: Vec<String> = e.parents.iter().map(&id).collect();
                out.extend_from_slice(format!("Merge: {}\n", parents.join(" ")).as_bytes());
            }
            let line = |out: &mut Vec<u8>, label: &str, sig: &Signature, presence: &Presence| {
                if presence.mail {
                    out.extend_from_slice(format!("{label}{}\n", person(sig)).as_bytes());
                }
            };
            match layout {
                Layout::Full => {
                    line(&mut out, "Author: ", e.author, e.author_presence);
                    line(&mut out, "Commit: ", e.committer, e.committer_presence);
                }
                Layout::Fuller => {
                    line(&mut out, "Author:     ", e.author, e.author_presence);
                    if e.author_presence.mail {
                        out.extend_from_slice(
                            format!(
                                "AuthorDate: {}\n",
                                style.date_of(e.author, e.author_presence)
                            )
                            .as_bytes(),
                        );
                    }
                    line(&mut out, "Commit:     ", e.committer, e.committer_presence);
                    if e.committer_presence.mail {
                        out.extend_from_slice(
                            format!(
                                "CommitDate: {}\n",
                                style.date_of(e.committer, e.committer_presence)
                            )
                            .as_bytes(),
                        );
                    }
                }
                _ => {
                    line(&mut out, "Author: ", e.author, e.author_presence);
                    if *layout == Layout::Medium && e.author_presence.mail {
                        out.extend_from_slice(
                            format!("Date:   {}\n", style.date_of(e.author, e.author_presence))
                                .as_bytes(),
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

/// The text of a `commit` line or oneline id, in the commit colour when colour is on.
fn push_commit_id(out: &mut Vec<u8>, text: &[u8], style: &Style<'_>) {
    match style.palette {
        Some(palette) => palette.paint(out, Shade::Commit, text),
        None => out.extend_from_slice(text),
    }
}

/// ` (HEAD -> main, tag: v1)`, painted piece by piece when colour is on.
fn push_decorations(out: &mut Vec<u8>, decorations: &[Decoration], style: &Style<'_>) {
    if decorations.is_empty() {
        return;
    }
    let Some(palette) = style.palette else {
        let labels = plain_labels(decorations);
        out.extend_from_slice(format!(" ({})", labels.join(", ")).as_bytes());
        return;
    };
    palette.paint(out, Shade::Commit, b" (");
    for (n, decoration) in decorations.iter().enumerate() {
        if n > 0 {
            palette.paint(out, Shade::Commit, b", ");
        }
        for (shade, text) in &decoration.pieces {
            palette.paint(out, *shade, text.as_bytes());
        }
    }
    palette.paint(out, Shade::Commit, b")");
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
    let sig_field = |sig: &Signature, presence: &Presence, field: u8| -> Option<Vec<u8>> {
        // git: no `<email>` leaves `%aN`/`%aE` unexpanded; no dated `+zone` leaves `%aI`/`%as`
        // unexpanded and every other date placeholder empty.
        if !presence.mail && matches!(field, b'N' | b'E') {
            return None;
        }
        if presence.date.is_none() && matches!(field, b'I' | b's') {
            return None;
        }
        if presence.date.is_none() && b"dDrit".contains(&field) {
            return Some(Vec::new());
        }
        Some(match field {
            b'n' | b'N' => sig.name.to_vec(),
            b'e' | b'E' => sig.email.to_vec(),
            b'd' => style.date_of(sig, presence).into_bytes(),
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
            b't' => presence.date.clone().unwrap_or_default(),
            _ => return None,
        })
    };
    if rest.len() >= 2 && (rest[0] == b'a' || rest[0] == b'c') && b"nNeEdDrIist".contains(&rest[1])
    {
        let (sig, presence) = if rest[0] == b'a' {
            (e.author, e.author_presence)
        } else {
            (e.committer, e.committer_presence)
        };
        return sig_field(sig, presence, rest[1]).map(|t| (2, t));
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
        b'D' => plain_labels(e.decorations).join(", ").into_bytes(),
        b'd' if e.decorations.is_empty() => Vec::new(),
        b'd' => format!(" ({})", plain_labels(e.decorations).join(", ")).into_bytes(),
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

#[cfg(test)]
mod tests {
    use super::{Presence, parse_color_spec};

    /// Expectations from `git log --format=%ad|%at` over the same identity lines (A8).
    #[test]
    fn presence_follows_split_ident_line() {
        let of = |ident: &[u8]| {
            let raw = [
                b"tree x\nauthor ",
                ident,
                b"\ncommitter C <c@x> 1 +0000\n\nm\n",
            ]
            .concat();
            Presence::of_header(&raw, b"author")
        };
        let dated = |digits: &[u8]| Presence {
            mail: true,
            date: Some(digits.to_vec()),
        };
        assert_eq!(of(b"Foo 1234 +0000"), Presence::default());
        assert_eq!(
            of(b"Foo <foo@x.com>"),
            Presence {
                mail: true,
                date: None
            }
        );
        assert_eq!(
            of(b"Foo <foo@x.com> 1234"),
            Presence {
                mail: true,
                date: None
            }
        );
        assert_eq!(of(b"Foo <foo@x.com> 1234 +0000"), dated(b"1234"));
        assert_eq!(
            of(b"Foo <foo@x.com> 99999999999999999999999 +0000"),
            dated(b"99999999999999999999999")
        );
        assert_eq!(
            Presence::of_header(b"tree x\n\nauthor A <a> 1 +0000\n", b"author"),
            Presence::default()
        );
    }

    /// Every expectation below was produced by `git -c color.diff.commit=<spec> log --color=always`.
    #[test]
    fn color_specs_match_what_git_prints() {
        let cases = [
            ("ul bold italic", Some("\x1b[1;3;4m")),
            ("reverse dim strike blink", Some("\x1b[2;5;7;9m")),
            ("red blue", Some("\x1b[31;44m")),
            ("bold", Some("\x1b[1m")),
            ("normal", Some("")),
            ("normal red", Some("\x1b[41m")),
            ("brightred", Some("\x1b[91m")),
            ("brightblack white", Some("\x1b[90;47m")),
            ("200", Some("\x1b[38;5;200m")),
            ("5 7", Some("\x1b[35;47m")),
            ("#ff8000", Some("\x1b[38;2;255;128;0m")),
            ("bold #00ff00 18", Some("\x1b[1;38;2;0;255;0;48;5;18m")),
            ("reset", Some("\x1b[m")),
            ("", Some("")),
            ("red green blue", None),
            ("bogus", None),
        ];
        for (spec, expected) in cases {
            assert_eq!(parse_color_spec(spec).as_deref(), expected, "{spec:?}");
        }
    }
}
