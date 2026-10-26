//! `for-each-ref`'s format language and filters, following git's `ref-filter.c`.
//!
//! A format string is a sequence of literal text, `%%`, `%xx` (a byte in hex) and `%(atom)`
//! placeholders. Atoms read the ref itself (`refname`, `objectname`, …), the commit or tag object
//! it names (`author*`, `committer*`, `tagger*`, `subject`, `body`, `contents*`, `tree`,
//! `parent`, …), the branch's upstream (`upstream[:track|:trackshort|:short|…]`) and the
//! formatting atoms (`if`/`then`/`else`/`end`, `align`, `color`). A leading `*` on an atom
//! dereferences an annotated tag once and reads the object behind it.

use std::collections::HashSet;

use rivet_core::object::{Commit, Signature, Tag};
use rivet_core::{ObjectId, ObjectKind, Repository};

/// What git prints after `error: malformed format string …` (exit status 129).
const USAGE: &str = r#"usage: git for-each-ref [--count=<count>] [--shell|--perl|--python|--tcl]
                                [(--sort=<key>)...] [--format=<format>]
                                [--include-root-refs] [--points-at=<object>]
                                [--merged[=<object>]] [--no-merged[=<object>]]
                                [--contains[=<object>]] [--no-contains[=<object>]]
                                [(--exclude=<pattern>)...] [--start-after=<marker>]
                                [ --stdin | (<pattern>...)]

    -s, --[no-]shell      quote placeholders suitably for shells
    -p, --[no-]perl       quote placeholders suitably for perl
    --[no-]python         quote placeholders suitably for python
    --[no-]tcl            quote placeholders suitably for Tcl
    --[no-]omit-empty     do not output a newline after empty formatted refs

    --[no-]count <n>      show only <n> matched refs
    --[no-]format <format>
                          format to use for the output
    --[no-]start-after <marker>
                          start iteration after the provided marker
    --[no-]color[=<when>] respect format colors
    --[no-]exclude <pattern>
                          exclude refs which match pattern
    --[no-]sort <key>     field name to sort on
    --[no-]points-at <object>
                          print only refs which points at the given object
    --merged <commit>     print only refs that are merged
    --no-merged <commit>  print only refs that are not merged
    --contains <commit>   print only refs which contain the commit
    --no-contains <commit>
                          print only refs which don't contain the commit
    --[no-]ignore-case    sorting and filtering are case insensitive
    --[no-]stdin          read reference patterns from stdin
    --[no-]include-root-refs
                          also include HEAD ref and pseudorefs
"#;

/// A fatal diagnostic: printed as `fatal: <text>`, exit status 128.
#[derive(Debug)]
pub(super) struct Fatal(pub String);

/// Text prefix marking a diagnostic that git reports as a usage error.
const MALFORMED: &str = "malformed format string ";

/// Text of the usage error raised when two different quoting styles are requested.
const TWO_STYLES: &str = "more than one quoting style?";

/// The `--shell` / `--perl` / `--python` / `--tcl` quoting of `%(atom)` values.
#[derive(Clone, Copy)]
enum Quote {
    Shell,
    Perl,
    Python,
    Tcl,
}

impl Quote {
    /// Git's `sq_quote_buf`, `perl_quote_buf`, `python_quote_buf` and `tcl_quote_buf`.
    fn apply(self, text: &str) -> String {
        let (open, close) = match self {
            Quote::Tcl => ('"', '"'),
            _ => ('\'', '\''),
        };
        let mut out = String::from(open);
        for c in text.chars() {
            match (self, c) {
                (Quote::Shell, '\'' | '!') => {
                    out.push_str("'\\");
                    out.push(c);
                    out.push('\'');
                }
                (Quote::Perl | Quote::Python, '\'' | '\\') => {
                    out.push('\\');
                    out.push(c);
                }
                (Quote::Python, '\n') => out.push_str("\\n"),
                (Quote::Tcl, '\x0c') => out.push_str("\\f"),
                (Quote::Tcl, '\r') => out.push_str("\\r"),
                (Quote::Tcl, '\n') => out.push_str("\\n"),
                (Quote::Tcl, '\t') => out.push_str("\\t"),
                (Quote::Tcl, '\x0b') => out.push_str("\\v"),
                (Quote::Tcl, '[' | ']' | '{' | '}' | '$' | '\\' | '"') => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out.push(close);
        out
    }
}

/// Like [`evaluate`], quoting each top-level placeholder (and each `%(if)`/`%(align)` group as a
/// whole) but not the literal text between them.
fn evaluate_quoted(
    nodes: &[Node],
    item: &RefItem,
    context: &Context,
    style: Quote,
) -> Eval<String> {
    let mut out = String::new();
    for node in nodes {
        let text = evaluate(std::slice::from_ref(node), item, context)?;
        match node {
            Node::Text(_) => out.push_str(&text),
            _ => out.push_str(&style.apply(&text)),
        }
    }
    Ok(out)
}

pub(super) type Eval<T> = std::result::Result<T, Fatal>;

fn fatal<T>(text: impl Into<String>) -> Eval<T> {
    Err(Fatal(text.into()))
}

impl From<anyhow::Error> for Fatal {
    fn from(error: anyhow::Error) -> Self {
        Fatal(error.to_string())
    }
}

impl From<rivet_core::Error> for Fatal {
    fn from(error: rivet_core::Error) -> Self {
        Fatal(error.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// One parsed element of a format string.
#[derive(Debug)]
pub(super) enum Node {
    Text(String),
    Atom(String),
    If {
        mode: IfMode,
        condition: Vec<Node>,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
    Align {
        width: usize,
        position: Position,
        body: Vec<Node>,
    },
}

#[derive(Debug)]
pub(super) enum IfMode {
    NonEmpty,
    Equals(String),
    NotEquals(String),
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Position {
    Left,
    Middle,
    Right,
}

enum Token {
    Text(String),
    Atom(String),
}

fn tokenize(format: &str) -> Eval<Vec<Token>> {
    let bytes = format.as_bytes();
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            let start = at;
            while at < bytes.len() && bytes[at] != b'%' {
                at += 1;
            }
            text.push_str(&format[start..at]);
            continue;
        }
        let rest = &format[at + 1..];
        if rest.starts_with('%') {
            text.push('%');
            at += 2;
            continue;
        }
        if let Some(inner) = rest.strip_prefix('(') {
            let Some(end) = inner.find(')') else {
                return fatal(format!("{MALFORMED}%({inner}"));
            };
            if !text.is_empty() {
                tokens.push(Token::Text(std::mem::take(&mut text)));
            }
            tokens.push(Token::Atom(inner[..end].to_string()));
            at += 2 + end + 1;
            continue;
        }
        let hex = rest.as_bytes();
        if hex.len() >= 2
            && let (Some(high), Some(low)) =
                ((hex[0] as char).to_digit(16), (hex[1] as char).to_digit(16))
        {
            text.push(char::from((high * 16 + low) as u8));
            at += 3;
            continue;
        }
        // A lone `%` is literal text.
        text.push('%');
        at += 1;
    }
    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }
    Ok(tokens)
}

/// Parse a format string into nodes, matching `%(if)`/`%(then)`/`%(else)`/`%(align)` with their
/// `%(end)`.
pub(super) fn parse_format(format: &str) -> Eval<Vec<Node>> {
    let tokens = tokenize(format)?;
    let mut at = 0;
    let (nodes, stop) = parse_nodes(&tokens, &mut at, &[])?;
    debug_assert!(stop.is_none());
    Ok(nodes)
}

/// Parse until one of `stops` (an atom name) or the end; returns the stop that was hit.
fn parse_nodes(
    tokens: &[Token],
    at: &mut usize,
    stops: &[&'static str],
) -> Eval<(Vec<Node>, Option<&'static str>)> {
    let mut nodes = Vec::new();
    while *at < tokens.len() {
        let token = &tokens[*at];
        *at += 1;
        let atom = match token {
            Token::Text(text) => {
                nodes.push(Node::Text(text.clone()));
                continue;
            }
            Token::Atom(atom) => atom,
        };
        let (name, argument) = match atom.split_once(':') {
            Some((name, argument)) => (name, Some(argument)),
            None => (atom.as_str(), None),
        };
        if let Some(stop) = stops.iter().find(|s| **s == name) {
            return Ok((nodes, Some(stop)));
        }
        match name {
            "if" => {
                let mode = match argument {
                    None => IfMode::NonEmpty,
                    Some(a) => match a.split_once('=') {
                        Some(("equals", value)) => IfMode::Equals(value.to_string()),
                        Some(("notequals", value)) => IfMode::NotEquals(value.to_string()),
                        _ => return fatal(format!("unrecognized %(if) argument: {a}")),
                    },
                };
                let (condition, stop) = parse_nodes(tokens, at, &["then", "else", "end"])?;
                if stop != Some("then") {
                    return fatal("format: %(if) atom used without a %(then) atom");
                }
                let (then, stop) = parse_nodes(tokens, at, &["then", "else", "end"])?;
                let otherwise = match stop {
                    Some("else") => {
                        let (otherwise, stop) = parse_nodes(tokens, at, &["then", "else", "end"])?;
                        match stop {
                            Some("end") => {}
                            Some("else") => {
                                return fatal("format: %(else) atom used more than once");
                            }
                            _ => return fatal("format: %(end) atom missing"),
                        }
                        otherwise
                    }
                    Some("end") => Vec::new(),
                    Some("then") => return fatal("format: %(then) atom used more than once"),
                    _ => return fatal("format: %(end) atom missing"),
                };
                nodes.push(Node::If {
                    mode,
                    condition,
                    then,
                    otherwise,
                });
            }
            "align" => {
                let (width, position) = parse_align(argument)?;
                let (body, stop) = parse_nodes(tokens, at, &["end"])?;
                if stop != Some("end") {
                    return fatal("format: %(end) atom missing");
                }
                nodes.push(Node::Align {
                    width,
                    position,
                    body,
                });
            }
            "then" => return fatal("format: %(then) atom used without a %(if) atom"),
            "else" => return fatal("format: %(else) atom used without a %(if) atom"),
            "end" => return fatal("format: %(end) atom used without corresponding atom"),
            _ => nodes.push(Node::Atom(atom.clone())),
        }
    }
    Ok((nodes, None))
}

fn parse_align(argument: Option<&str>) -> Eval<(usize, Position)> {
    let Some(argument) = argument else {
        return fatal("expected format: %(align:<width>,<position>)");
    };
    let mut width = None;
    let mut position = Position::Left;
    for (index, part) in argument.split(',').enumerate() {
        if let Some(value) = part.strip_prefix("width=") {
            width = value.parse().ok();
        } else if let Some(value) = part.strip_prefix("position=") {
            position = parse_position(value)?;
        } else if index == 0 && part.bytes().all(|b| b.is_ascii_digit()) && !part.is_empty() {
            width = part.parse().ok();
        } else if matches!(part, "left" | "right" | "middle") {
            position = parse_position(part)?;
        } else {
            return fatal(format!("unrecognized %(align) argument: {part}"));
        }
    }
    match width {
        Some(width) => Ok((width, position)),
        None => fatal("positive width expected with the %(align) atom"),
    }
}

fn parse_position(value: &str) -> Eval<Position> {
    match value {
        "left" => Ok(Position::Left),
        "right" => Ok(Position::Right),
        "middle" => Ok(Position::Middle),
        other => fatal(format!("unrecognized position:{other}")),
    }
}

// ---------------------------------------------------------------------------------------------
// The ref being formatted
// ---------------------------------------------------------------------------------------------

/// A commit, with only what atoms read.
pub(super) struct CommitFacts {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Signature,
    committer: Signature,
    message: Vec<u8>,
}

/// An annotated tag object.
pub(super) struct TagFacts {
    target: ObjectId,
    target_kind: ObjectKind,
    name: String,
    tagger: Signature,
    message: Vec<u8>,
    signature: Vec<u8>,
}

/// An object as atoms see it.
pub(super) struct Facts {
    pub oid: ObjectId,
    kind: ObjectKind,
    size: usize,
    commit: Option<CommitFacts>,
    tag: Option<TagFacts>,
}

impl Facts {
    pub(super) fn load(repository: &Repository, oid: ObjectId) -> Eval<Facts> {
        let object = repository.odb().read(&oid)?;
        let mut facts = Facts {
            oid,
            kind: object.kind(),
            size: object.data().len(),
            commit: None,
            tag: None,
        };
        match object.kind() {
            ObjectKind::Commit => {
                let commit = Commit::parse(repository.format(), object.data())?;
                facts.commit = Some(CommitFacts {
                    tree: commit.tree,
                    parents: commit.parents.clone(),
                    author: commit.author.clone(),
                    committer: commit.committer.clone(),
                    message: commit.message.to_vec(),
                });
            }
            ObjectKind::Tag => {
                let tag = Tag::parse(repository.format(), object.data())?;
                facts.tag = Some(TagFacts {
                    target: tag.object,
                    target_kind: tag.kind,
                    name: tag.name.to_string(),
                    tagger: tag.tagger.clone(),
                    message: tag.message.to_vec(),
                    signature: tag.signature.map(|s| s.to_vec()).unwrap_or_default(),
                });
            }
            _ => {}
        }
        Ok(facts)
    }

    /// The object behind an annotated tag, looked through nested tags down to the first
    /// non-tag object; `None` when this is not a tag.
    pub(super) fn deref(&self, repository: &Repository) -> Eval<Option<Facts>> {
        let Some(tag) = &self.tag else {
            return Ok(None);
        };
        let mut next = tag.target;
        loop {
            let facts = Facts::load(repository, next)?;
            match &facts.tag {
                Some(inner) => next = inner.target,
                None => return Ok(Some(facts)),
            }
        }
    }

    fn signature(&self, which: &str) -> Option<&Signature> {
        match which {
            "author" => self.commit.as_ref().map(|c| &c.author),
            "committer" => self.commit.as_ref().map(|c| &c.committer),
            "tagger" => self.tag.as_ref().map(|t| &t.tagger),
            "creator" => self
                .commit
                .as_ref()
                .map(|c| &c.committer)
                .or_else(|| self.tag.as_ref().map(|t| &t.tagger)),
            _ => None,
        }
    }

    /// The full message: a commit's message, or a tag's message followed by its signature.
    fn contents(&self) -> Vec<u8> {
        if let Some(commit) = &self.commit {
            commit.message.clone()
        } else if let Some(tag) = &self.tag {
            let mut all = tag.message.clone();
            all.extend_from_slice(&tag.signature);
            all
        } else {
            Vec::new()
        }
    }

    fn signature_block(&self) -> Vec<u8> {
        self.tag
            .as_ref()
            .map(|t| t.signature.clone())
            .unwrap_or_default()
    }

    /// The message without its signature.
    fn message_only(&self) -> Vec<u8> {
        if let Some(commit) = &self.commit {
            commit.message.clone()
        } else if let Some(tag) = &self.tag {
            tag.message.clone()
        } else {
            Vec::new()
        }
    }
}

/// `find_subpos`: split a message into its subject paragraph and the body behind it.
fn split_message(message: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut start = 0;
    while start < message.len() && message[start] == b'\n' {
        start += 1;
    }
    let rest = &message[start..];
    let end = rest
        .windows(2)
        .position(|w| w == b"\n\n")
        .unwrap_or(rest.len());
    let mut subject = &rest[..end];
    while subject.last() == Some(&b'\n') {
        subject = &subject[..subject.len() - 1];
    }
    let mut body_start = end;
    while body_start < rest.len() && rest[body_start] == b'\n' {
        body_start += 1;
    }
    (
        subject
            .iter()
            .map(|b| if *b == b'\n' { b' ' } else { *b })
            .collect(),
        rest[body_start..].to_vec(),
    )
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

/// A branch's upstream as `%(upstream)` reports it.
pub(super) struct Upstream {
    /// The configured `branch.<name>.remote`.
    pub remote: String,
    /// The configured `branch.<name>.merge`.
    pub merge: String,
    /// The remote-tracking (or local) ref it maps to.
    pub tracking: String,
}

pub(super) struct Context<'a> {
    /// `--color`: whether `%(color:…)` produces escape codes.
    pub color: bool,
    pub repository: &'a Repository,
    pub all_refs: &'a HashSet<&'a str>,
    pub head_ref: Option<String>,
    pub now: i64,
}

pub(super) struct RefItem {
    pub name: String,
    pub facts: Facts,
}

/// The value of an atom, with the numeric form sorting uses.
pub(super) enum Value {
    Text(String),
    Number(i64, String),
}

impl Value {
    fn text(self) -> String {
        match self {
            Value::Text(text) | Value::Number(_, text) => text,
        }
    }
}

pub(super) fn evaluate(nodes: &[Node], item: &RefItem, context: &Context) -> Eval<String> {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Atom(atom) => out.push_str(&atom_value(atom, item, context)?.text()),
            Node::If {
                mode,
                condition,
                then,
                otherwise,
            } => {
                let value = evaluate(condition, item, context)?;
                let hit = match mode {
                    IfMode::NonEmpty => !value.trim().is_empty(),
                    IfMode::Equals(wanted) => value.trim() == wanted,
                    IfMode::NotEquals(wanted) => value.trim() != wanted,
                };
                out.push_str(&evaluate(
                    if hit { then } else { otherwise },
                    item,
                    context,
                )?);
            }
            Node::Align {
                width,
                position,
                body,
            } => {
                let text = evaluate(body, item, context)?;
                let length = text.chars().count();
                if length >= *width {
                    out.push_str(&text);
                } else {
                    let pad = width - length;
                    let (left, right) = match position {
                        Position::Left => (0, pad),
                        Position::Right => (pad, 0),
                        Position::Middle => (pad / 2, pad - pad / 2),
                    };
                    out.push_str(&" ".repeat(left));
                    out.push_str(&text);
                    out.push_str(&" ".repeat(right));
                }
            }
        }
    }
    Ok(out)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn kind_text(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// The value of one `%(atom)` for one ref.
pub(super) fn atom_value(atom: &str, item: &RefItem, context: &Context) -> Eval<Value> {
    let (deref, atom) = match atom.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, atom),
    };
    let (field, modifier) = match atom.split_once(':') {
        Some((field, modifier)) => (field, Some(modifier)),
        None => (atom, None),
    };
    // Fields of the ref itself are not affected by `*`.
    let dereferenced;
    let facts: Option<&Facts> = if deref {
        dereferenced = item.facts.deref(context.repository)?;
        dereferenced.as_ref()
    } else {
        Some(&item.facts)
    };
    let text = |s: String| Ok(Value::Text(s));
    match field {
        "refname" => {
            if deref {
                return text(String::new());
            }
            text(refname_value(&item.name, modifier, context)?)
        }
        "objectname" => {
            let Some(facts) = facts else {
                return text(String::new());
            };
            let hex = facts.oid.to_string();
            match modifier {
                None => text(hex),
                Some("short") => text(super::short_oid(&facts.oid)),
                Some(m) if m.starts_with("short=") => {
                    let n: usize = m[6..].parse().unwrap_or(7);
                    text(hex.chars().take(n.max(4)).collect())
                }
                Some(m) => fatal(format!("unrecognized %(objectname) argument: {m}")),
            }
        }
        "objecttype" => text(
            facts
                .map(|f| kind_text(f.kind).to_string())
                .unwrap_or_default(),
        ),
        "objectsize" => Ok(match facts {
            Some(f) => Value::Number(f.size as i64, f.size.to_string()),
            None => Value::Number(0, String::new()),
        }),
        "tree" => text(
            facts
                .and_then(|f| f.commit.as_ref())
                .map(|c| c.tree.to_string())
                .unwrap_or_default(),
        ),
        "parent" => text(
            facts
                .and_then(|f| f.commit.as_ref())
                .map(|c| {
                    c.parents
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
        ),
        "numparent" => Ok(match facts.and_then(|f| f.commit.as_ref()) {
            Some(c) => Value::Number(c.parents.len() as i64, c.parents.len().to_string()),
            None => Value::Number(0, String::new()),
        }),
        "object" => text(
            facts
                .and_then(|f| f.tag.as_ref())
                .map(|t| t.target.to_string())
                .unwrap_or_default(),
        ),
        "type" => text(
            facts
                .and_then(|f| f.tag.as_ref())
                .map(|t| kind_text(t.target_kind).to_string())
                .unwrap_or_default(),
        ),
        "tag" => text(
            facts
                .and_then(|f| f.tag.as_ref())
                .map(|t| t.name.clone())
                .unwrap_or_default(),
        ),
        "author" | "committer" | "tagger" | "creator" => {
            let sig = facts.and_then(|f| f.signature(field));
            text(match sig {
                Some(sig) => format!(
                    "{} <{}> {} {}",
                    sig.name,
                    sig.email,
                    sig.when,
                    tz_text(sig.tz_offset_minutes)
                ),
                None => String::new(),
            })
        }
        "authorname" | "committername" | "taggername" => {
            let who = field.trim_end_matches("name");
            text(
                facts
                    .and_then(|f| f.signature(who))
                    .map(|s| s.name.to_string())
                    .unwrap_or_default(),
            )
        }
        "authoremail" | "committeremail" | "taggeremail" => {
            let who = field.trim_end_matches("email");
            let Some(sig) = facts.and_then(|f| f.signature(who)) else {
                return text(String::new());
            };
            let email = sig.email.to_string();
            match modifier {
                None => text(format!("<{email}>")),
                Some("trim") => text(email),
                Some("localpart") => text(email.split('@').next().unwrap_or("").to_string()),
                Some(m) => fatal(format!("unrecognized email option: {m}")),
            }
        }
        "authordate" | "committerdate" | "taggerdate" | "creatordate" => {
            let who = field.trim_end_matches("date");
            let Some(sig) = facts.and_then(|f| f.signature(who)) else {
                return Ok(Value::Number(0, String::new()));
            };
            Ok(Value::Number(
                sig.when,
                date_text(sig, modifier, context.now)?,
            ))
        }
        "subject" | "body" | "contents" => {
            let Some(facts) = facts else {
                return text(String::new());
            };
            let (what, argument) = if field == "contents" {
                match modifier {
                    None => ("contents", None),
                    Some(m) => match m.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (m, None),
                    },
                }
            } else {
                (field, None)
            };
            let (subject, body) = split_message(&facts.message_only());
            match what {
                "contents" => {
                    let all = facts.contents();
                    let start = all.iter().take_while(|b| **b == b'\n').count();
                    text(lossy(&all[start..]))
                }
                "subject" => text(lossy(&subject)),
                "body" => text(lossy(&body)),
                "signature" => text(lossy(&facts.signature_block())),
                "lines" => {
                    let count: usize = argument.and_then(|a| a.parse().ok()).unwrap_or(0);
                    let message = facts.message_only();
                    let mut start = 0;
                    while start < message.len() && message[start] == b'\n' {
                        start += 1;
                    }
                    // Continuation lines are indented by four spaces, as git does.
                    let lines: Vec<String> = lossy(&message[start..])
                        .lines()
                        .take(count)
                        .enumerate()
                        .map(|(i, line)| {
                            if i == 0 {
                                line.to_string()
                            } else {
                                format!("    {line}")
                            }
                        })
                        .collect();
                    text(lines.join("\n"))
                }
                other => fatal(format!("unrecognized %(contents) argument: {other}")),
            }
        }
        "upstream" | "push" => {
            if deref {
                return text(String::new());
            }
            text(upstream_value(field, modifier, item, context)?)
        }
        "symref" => {
            if deref {
                return text(String::new());
            }
            let found = if item.name.starts_with("refs/") {
                context
                    .repository
                    .refs()
                    .list(b"refs/")
                    .map_err(|e| Fatal(e.to_string()))?
                    .into_iter()
                    .find(|(n, _)| n.as_bytes() == item.name.as_bytes())
                    .map(|(_, target)| target)
            } else {
                // A root ref (`HEAD`, `ORIG_HEAD`, …) lives directly in the git directory.
                rivet_core::RefName::from_bytes(item.name.as_bytes())
                    .ok()
                    .and_then(|name| context.repository.refs().read(&name).ok())
            };
            text(match found {
                Some(rivet_core::RefTarget::Symbolic(target)) => lossy(target.as_bytes()),
                _ => String::new(),
            })
        }
        "HEAD" => text(
            if context.head_ref.as_deref() == Some(item.name.as_str()) {
                "*"
            } else {
                " "
            }
            .to_string(),
        ),
        "color" => text(color_value(modifier, context)?),
        "flag" | "worktreepath" | "rest" | "deltabase" => text(String::new()),
        other => fatal(format!("unknown field name: {other}")),
    }
}

fn tz_text(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.abs();
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

fn refname_value(name: &str, modifier: Option<&str>, context: &Context) -> Eval<String> {
    let strip = |n: usize| name.split('/').skip(n).collect::<Vec<_>>().join("/");
    match modifier {
        None => Ok(name.to_string()),
        Some("short") => Ok(super::query::shorten(name, context.all_refs)),
        Some(m) if m.starts_with("lstrip=") || m.starts_with("strip=") => {
            let n: i64 = m
                .split_once('=')
                .and_then(|(_, n)| n.parse().ok())
                .unwrap_or(0);
            let parts: Vec<&str> = name.split('/').collect();
            if n >= 0 {
                Ok(strip(n as usize))
            } else {
                let keep = (-n) as usize;
                Ok(parts[parts.len().saturating_sub(keep)..].join("/"))
            }
        }
        Some(m) if m.starts_with("rstrip=") => {
            let n: i64 = m
                .split_once('=')
                .and_then(|(_, n)| n.parse().ok())
                .unwrap_or(0);
            let parts: Vec<&str> = name.split('/').collect();
            if n >= 0 {
                Ok(parts[..parts.len().saturating_sub(n as usize)].join("/"))
            } else {
                Ok(parts[..((-n) as usize).min(parts.len())].join("/"))
            }
        }
        Some(m) => fatal(format!("unrecognized %(refname) argument: {m}")),
    }
}

fn date_text(sig: &Signature, modifier: Option<&str>, now: i64) -> Eval<String> {
    let style = modifier.unwrap_or("default");
    if let Some(format) = style.strip_prefix("format:") {
        return Ok(strftime(format, sig.when, sig.tz_offset_minutes));
    }
    match rivet_core::ops::format_date_style(style, sig.when, sig.tz_offset_minutes, now) {
        Some(text) => Ok(text),
        None => fatal(format!("unknown date format {style}")),
    }
}

/// A small `strftime` over a timestamp in the signature's own offset.
fn strftime(format: &str, when: i64, offset_minutes: i32) -> String {
    let local = when + i64::from(offset_minutes) * 60;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year_of_era = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + i64::from(month <= 2);
    let weekday = (days + 4).rem_euclid(7) as usize;
    let (hour, minute, second) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    const CUMULATIVE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = CUMULATIVE[(month - 1) as usize] + day + i64::from(leap && month > 2);
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            out.push('%');
            break;
        };
        match spec {
            'Y' => out.push_str(&year.to_string()),
            'C' => out.push_str(&format!("{:02}", year / 100)),
            'y' => out.push_str(&format!("{:02}", year.rem_euclid(100))),
            'm' => out.push_str(&format!("{month:02}")),
            'd' => out.push_str(&format!("{day:02}")),
            'e' => out.push_str(&format!("{day:2}")),
            'H' => out.push_str(&format!("{hour:02}")),
            'I' => out.push_str(&format!("{:02}", (hour + 11) % 12 + 1)),
            'M' => out.push_str(&format!("{minute:02}")),
            'S' => out.push_str(&format!("{second:02}")),
            'p' => out.push_str(if hour < 12 { "AM" } else { "PM" }),
            'a' => out.push_str(&DAYS[weekday][..3]),
            'A' => out.push_str(DAYS[weekday]),
            'b' | 'h' => out.push_str(&MONTHS[(month - 1) as usize][..3]),
            'B' => out.push_str(MONTHS[(month - 1) as usize]),
            'j' => out.push_str(&format!("{yday:03}")),
            'u' => out.push_str(&(if weekday == 0 { 7 } else { weekday }).to_string()),
            'w' => out.push_str(&weekday.to_string()),
            'z' => out.push_str(&tz_text(offset_minutes)),
            's' => out.push_str(&when.to_string()),
            'F' => out.push_str(&format!("{year}-{month:02}-{day:02}")),
            'T' => out.push_str(&format!("{hour:02}:{minute:02}:{second:02}")),
            'R' => out.push_str(&format!("{hour:02}:{minute:02}")),
            'D' => out.push_str(&format!("{month:02}/{day:02}/{:02}", year.rem_euclid(100))),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn color_value(modifier: Option<&str>, context: &Context) -> Eval<String> {
    let Some(spec) = modifier else {
        return fatal("expected format: %(color:<color>)");
    };
    let enabled = context.color;
    if spec == "reset" {
        return Ok(if enabled { "\x1b[m" } else { "" }.to_string());
    }
    let mut attributes: Vec<String> = Vec::new();
    let mut colors: Vec<String> = Vec::new();
    for word in spec.split_whitespace() {
        let color = |base: u32, name: &str| -> Option<u32> {
            Some(
                base + match name {
                    "black" => 0,
                    "red" => 1,
                    "green" => 2,
                    "yellow" => 3,
                    "blue" => 4,
                    "magenta" => 5,
                    "cyan" => 6,
                    "white" => 7,
                    _ => return None,
                },
            )
        };
        match word {
            "bold" | "dim" | "ul" | "blink" | "reverse" | "italic" | "strike" => {
                attributes.push(word.to_string())
            }
            "normal" => {}
            other => {
                let base = if colors.is_empty() { 30 } else { 40 };
                match color(base, other) {
                    Some(code) => colors.push(code.to_string()),
                    None => {
                        eprintln!("error: invalid color value: {other}");
                        return fatal(format!("unrecognized color: %(color:{spec})"));
                    }
                }
            }
        }
    }
    // Git emits attributes in a fixed order, whatever order they were named in.
    const ORDER: [(&str, &str); 7] = [
        ("bold", "1"),
        ("dim", "2"),
        ("ul", "4"),
        ("blink", "5"),
        ("reverse", "7"),
        ("italic", "3"),
        ("strike", "9"),
    ];
    let mut attributes: Vec<String> = ORDER
        .iter()
        .filter(|(name, _)| attributes.iter().any(|a| a == name))
        .map(|(_, code)| (*code).to_string())
        .collect();
    attributes.extend(colors);
    if attributes.is_empty() || !enabled {
        return Ok(String::new());
    }
    Ok(format!("\x1b[{}m", attributes.join(";")))
}

/// What the `branch.<name>.*` configuration says about `refs/heads/<name>`.
pub(super) fn upstream_of(repository: &Repository, refname: &str) -> Option<Upstream> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let (remote, merge) = repository.branch_upstream(branch).ok()??;
    let tracking = if remote == "." {
        merge.clone()
    } else {
        let config = repository.remote_config(&remote).ok()??;
        config
            .fetch
            .iter()
            .find_map(|spec| spec.map(merge.as_bytes()))?
            .to_string()
    };
    Some(Upstream {
        remote,
        merge,
        tracking,
    })
}

fn upstream_value(
    field: &str,
    modifier: Option<&str>,
    item: &RefItem,
    context: &Context,
) -> Eval<String> {
    if field == "push" {
        return push_value(modifier, item, context);
    }
    let Some(upstream) = upstream_of(context.repository, &item.name) else {
        return Ok(String::new());
    };
    let mut options = modifier.unwrap_or("").split(',').filter(|s| !s.is_empty());
    let first = options.next();
    let nobracket = options.any(|o| o == "nobracket");
    match first {
        None => Ok(upstream.tracking),
        Some("short") => Ok(super::query::shorten(&upstream.tracking, context.all_refs)),
        Some("remotename") => Ok(upstream.remote),
        Some("remoteref") => Ok(upstream.merge),
        Some(kind @ ("track" | "trackshort")) => {
            let nobracket = nobracket || modifier.is_some_and(|m| m.ends_with(",nobracket"));
            let Some(counts) = ahead_behind(context, &item.facts.oid, &upstream.tracking)? else {
                return Ok(if kind == "track" {
                    if nobracket { "gone" } else { "[gone]" }.to_string()
                } else {
                    String::new()
                });
            };
            let (ahead, behind) = counts;
            if kind == "trackshort" {
                return Ok(match (ahead, behind) {
                    (0, 0) => "=",
                    (_, 0) => ">",
                    (0, _) => "<",
                    _ => "<>",
                }
                .to_string());
            }
            let inner = match (ahead, behind) {
                (0, 0) => return Ok(String::new()),
                (a, 0) => format!("ahead {a}"),
                (0, b) => format!("behind {b}"),
                (a, b) => format!("ahead {a}, behind {b}"),
            };
            Ok(if nobracket {
                inner
            } else {
                format!("[{inner}]")
            })
        }
        Some(other) => fatal(format!("unrecognized %(upstream) argument: {other}")),
    }
}

fn push_value(modifier: Option<&str>, item: &RefItem, context: &Context) -> Eval<String> {
    let Some(branch) = item.name.strip_prefix("refs/heads/") else {
        return Ok(String::new());
    };
    let config = context.repository.config();
    let get = |key: String| -> Option<String> {
        config
            .get(bstr::BStr::new(key.as_bytes()))
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(v).into_owned())
    };
    let remote = get(format!("branch.{branch}.pushremote"))
        .or_else(|| get("remote.pushdefault".to_string()))
        .or_else(|| get(format!("branch.{branch}.remote")))
        .unwrap_or_default();
    if modifier == Some("remotename") {
        return Ok(remote);
    }
    // `push.default=simple` (the default) pushes to the upstream's name and needs the upstream to
    // carry the branch's own name; `current` always pushes to the same name.
    let same_name = get(format!("branch.{branch}.merge")).as_deref()
        == Some(format!("refs/heads/{branch}").as_str());
    let allowed = match get("push.default".to_string()).as_deref() {
        Some("current") => true,
        Some("nothing") => false,
        _ => same_name,
    };
    if remote.is_empty() || remote == "." || !allowed {
        return match modifier {
            None | Some("short") => Ok(String::new()),
            Some(other) => fatal(format!("unrecognized %(push) argument: {other}")),
        };
    }
    let tracking = format!("refs/remotes/{remote}/{branch}");
    match modifier {
        None => Ok(tracking),
        Some("short") => Ok(super::query::shorten(&tracking, context.all_refs)),
        Some(other) => fatal(format!("unrecognized %(push) argument: {other}")),
    }
}

/// `(ahead, behind)` of `oid` against the ref `other`, or `None` when that ref does not exist.
fn ahead_behind(context: &Context, oid: &ObjectId, other: &str) -> Eval<Option<(usize, usize)>> {
    let repository = context.repository;
    let Ok(name) = rivet_core::RefName::from_bytes(other.as_bytes()) else {
        return Ok(None);
    };
    let Ok(upstream_oid) = repository.refs().resolve_oid(&name) else {
        return Ok(None);
    };
    let count = |from: ObjectId, exclude: ObjectId| -> Eval<usize> {
        let mut options = rivet_core::revwalk::WalkOptions::new();
        options.exclude(exclude);
        Ok(rivet_core::revwalk::walk_oids(repository.odb(), &[from], &options)?.len())
    };
    let tip = repository
        .refs()
        .peel_oid(*oid, repository.odb())
        .unwrap_or(*oid);
    let upstream_tip = repository
        .refs()
        .peel_oid(upstream_oid, repository.odb())
        .unwrap_or(upstream_oid);
    Ok(Some((count(tip, upstream_tip)?, count(upstream_tip, tip)?)))
}

/// The value a `--sort` key orders by.
pub(super) enum SortValue {
    Text(String),
    Number(i64),
}

pub(super) fn sort_value(key: &str, item: &RefItem, context: &Context) -> Eval<SortValue> {
    Ok(match atom_value(key, item, context)? {
        Value::Text(text) => SortValue::Text(text),
        Value::Number(number, _) => SortValue::Number(number),
    })
}

/// git's `versioncmp` (glibc `strverscmp`) with no `versionsort.suffix` configured.
pub(super) fn versioncmp(left: &str, right: &str) -> Ordering {
    const CMP: i8 = 2;
    const LEN: i8 = 3;
    const NEXT_STATE: [usize; 12] = [0, 3, 9, 0, 3, 3, 0, 6, 6, 0, 6, 9];
    const RESULT: [i8; 36] = [
        CMP, CMP, CMP, CMP, LEN, CMP, CMP, CMP, CMP, //
        CMP, -1, -1, 1, LEN, LEN, 1, LEN, LEN, //
        CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, //
        CMP, 1, 1, -1, CMP, CMP, -1, CMP, CMP,
    ];
    let (a, b) = (left.as_bytes(), right.as_bytes());
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let class = |c: u8| usize::from(c == b'0') + usize::from(c.is_ascii_digit());
    let mut i = 0;
    let (mut c1, mut c2) = (at(a, 0), at(b, 0));
    let mut state = class(c1);
    while c1 == c2 {
        if c1 == 0 {
            return Ordering::Equal;
        }
        state = NEXT_STATE[state];
        i += 1;
        c1 = at(a, i);
        c2 = at(b, i);
        state += class(c1);
    }
    let diff = i32::from(c1) - i32::from(c2);
    let by_diff = diff.cmp(&0);
    match RESULT[state * 3 + class(c2)] {
        CMP => by_diff,
        LEN => {
            let (mut p1, mut p2) = (i + 1, i + 1);
            while at(a, p1).is_ascii_digit() {
                p1 += 1;
                if !at(b, p2).is_ascii_digit() {
                    return Ordering::Greater;
                }
                p2 += 1;
            }
            if at(b, p2).is_ascii_digit() {
                Ordering::Less
            } else {
                by_diff
            }
        }
        r => r.cmp(&0),
    }
}

// ---------------------------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------------------------

use std::cmp::Ordering;

use super::query::ForEachRefArgs;
use crate::cli::GlobalArgs;

fn write_stdout(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|_| stdout.flush()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

/// Handler for `rivet for-each-ref`.
pub(super) fn run(global: &GlobalArgs, args: &ForEachRefArgs) -> anyhow::Result<i32> {
    match run_inner(global, args)? {
        Ok(code) => Ok(code),
        Err(Fatal(text)) if text.starts_with(MALFORMED) || text == TWO_STYLES => {
            eprintln!("error: {text}");
            eprintln!("{USAGE}");
            Ok(129)
        }
        Err(Fatal(text)) => {
            eprintln!("fatal: {text}");
            Ok(128)
        }
    }
}

/// The refs git lists under `--include-root-refs`: files directly in the git directory named
/// `HEAD` or made of `[A-Z_]` ending in `_HEAD`, except `FETCH_HEAD` and `MERGE_HEAD`. A file
/// that is not a ref gets git's warning, a symref that points nowhere is left out silently.
fn root_refs(repository: &Repository) -> Vec<(String, ObjectId)> {
    let Ok(entries) = std::fs::read_dir(repository.refs().git_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let shaped = name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
            && (name == "HEAD" || name.ends_with("_HEAD"));
        if !shaped
            || name == "FETCH_HEAD"
            || name == "MERGE_HEAD"
            || !entry
                .file_type()
                .is_ok_and(|t| t.is_file() || t.is_symlink())
        {
            continue;
        }
        let Ok(refname) = rivet_core::RefName::from_bytes(name.as_bytes()) else {
            continue;
        };
        match repository.refs().read(&refname) {
            Ok(_) => {
                if let Ok(oid) = repository.refs().resolve_oid(&refname) {
                    out.push((name, oid));
                }
            }
            Err(_) => eprintln!("warning: ignoring broken ref {name}"),
        }
    }
    out
}

fn run_inner(global: &GlobalArgs, args: &ForEachRefArgs) -> anyhow::Result<Eval<i32>> {
    let repository = super::open(global)?;
    if args.start_after.is_some() && !args.sort.is_empty() {
        return Ok(Err(Fatal(
            "cannot use --start-after with custom sort options".to_string(),
        )));
    }
    if args.stdin && !args.patterns.is_empty() {
        return Ok(Err(Fatal(
            "unknown arguments supplied with --stdin".to_string(),
        )));
    }
    // Under `--stdin` an empty list means "every ref", but a list of blank lines matches nothing.
    let mut patterns = args.patterns.clone();
    if args.stdin {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        patterns = input
            .lines()
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .collect();
    }
    if args.start_after.is_some() && !patterns.is_empty() {
        return Ok(Err(Fatal(
            "cannot use --start-after with patterns".to_string(),
        )));
    }
    let mut refs = super::query::refs_sorted(&repository)?;
    if args.include_root_refs {
        refs.extend(root_refs(&repository));
        refs.sort();
    }
    if let Some(marker) = &args.start_after {
        refs.retain(|(name, _)| name.as_str() > marker.as_str());
    }
    refs.retain(|(name, _)| {
        (patterns.is_empty()
            || patterns
                .iter()
                .any(|p| !p.is_empty() && super::query::ref_pattern_matches(p, name)))
            && !args
                .exclude
                .iter()
                .any(|p| super::query::ref_pattern_matches(p, name))
    });

    let styles: Vec<Quote> = [
        (args.shell, Quote::Shell),
        (args.perl, Quote::Perl),
        (args.python, Quote::Python),
        (args.tcl, Quote::Tcl),
    ]
    .into_iter()
    .filter_map(|(on, style)| on.then_some(style))
    .collect();
    if styles.len() > 1 {
        return Ok(Err(Fatal(TWO_STYLES.to_string())));
    }
    let quote = styles.first().copied();

    let format = args
        .format
        .clone()
        .unwrap_or_else(|| "%(objectname) %(objecttype)\t%(refname)".to_string());
    let nodes = match parse_format(&format) {
        Ok(nodes) => nodes,
        Err(fatal) => return Ok(Err(fatal)),
    };

    let all_names: Vec<String> = super::query::refs_sorted(&repository)?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let all: HashSet<&str> = all_names.iter().map(String::as_str).collect();
    let color = {
        use std::io::IsTerminal as _;
        match args.color.as_deref() {
            Some("always") => true,
            Some("auto" | "tty") => std::io::stdout().is_terminal(),
            _ => false,
        }
    };
    let context = Context {
        color,
        repository: &repository,
        all_refs: &all,
        head_ref: repository.head()?.branch().map(|b| b.to_string()),
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };

    // `--merged` and friends are resolved once, to commits.
    let commits = |specs: &[String]| -> std::result::Result<Vec<ObjectId>, String> {
        specs
            .iter()
            .map(|spec| {
                super::resolve_commit(&repository, spec)
                    .map_err(|_| format!("malformed object name {spec}"))
            })
            .collect()
    };
    let merged = match commits(&args.merged) {
        Ok(commits) => commits,
        Err(text) => return Ok(Err(Fatal(text))),
    };
    let no_merged = match commits(&args.no_merged) {
        Ok(commits) => commits,
        Err(text) => return Ok(Err(Fatal(text))),
    };
    let contains = match commits(&args.contains) {
        Ok(commits) => commits,
        Err(text) => {
            eprintln!("error: {text}");
            return Ok(Ok(129));
        }
    };
    let no_contains = match commits(&args.no_contains) {
        Ok(commits) => commits,
        Err(text) => {
            eprintln!("error: {text}");
            return Ok(Ok(129));
        }
    };
    let points_at = args
        .points_at
        .iter()
        .map(|spec| {
            super::resolve_rev(&repository, spec)
                .map_err(|_| Fatal(format!("malformed object name '{spec}'")))
        })
        .collect::<std::result::Result<Vec<_>, Fatal>>();
    let points_at = match points_at {
        Ok(oids) => oids,
        Err(Fatal(text)) => {
            eprintln!("error: {text}");
            return Ok(Ok(129));
        }
    };

    let mut items: Vec<RefItem> = Vec::new();
    for (name, oid) in refs {
        let facts = match Facts::load(&repository, oid) {
            Ok(facts) => facts,
            Err(fatal) => return Ok(Err(fatal)),
        };
        if !points_at.is_empty() {
            let direct = points_at.contains(&oid);
            // Every level of a nested tag counts: a tag "points at" what it wraps.
            let mut tagged = false;
            let mut level = facts.tag.as_ref().map(|tag| tag.target);
            while let Some(target) = level {
                if points_at.contains(&target) {
                    tagged = true;
                    break;
                }
                level = Facts::load(&repository, target)
                    .ok()
                    .and_then(|inner| inner.tag.as_ref().map(|tag| tag.target));
            }
            if !direct && !tagged {
                continue;
            }
        }
        let wants_commit = !merged.is_empty()
            || !no_merged.is_empty()
            || !contains.is_empty()
            || !no_contains.is_empty();
        if wants_commit {
            // git's commit filters drop the root refs outright.
            if !name.starts_with("refs/") {
                continue;
            }
            // Annotated tags are looked through; anything that is not a commit is left out.
            let tip = repository
                .refs()
                .peel_oid(oid, repository.odb())
                .unwrap_or(oid);
            let is_commit = repository
                .odb()
                .read(&tip)
                .is_ok_and(|o| o.kind() == ObjectKind::Commit);
            if !is_commit {
                continue;
            }
            let reaches = |from: ObjectId, to: ObjectId| repository.is_ancestor(from, to);
            if !merged.is_empty() {
                let mut any = false;
                for commit in &merged {
                    any |= reaches(tip, *commit)?;
                }
                if !any {
                    continue;
                }
            }
            if !no_merged.is_empty() {
                let mut any = false;
                for commit in &no_merged {
                    any |= reaches(tip, *commit)?;
                }
                if any {
                    continue;
                }
            }
            if !contains.is_empty() {
                let mut any = false;
                for commit in &contains {
                    any |= reaches(*commit, tip)?;
                }
                if !any {
                    continue;
                }
            }
            if !no_contains.is_empty() {
                let mut any = false;
                for commit in &no_contains {
                    any |= reaches(*commit, tip)?;
                }
                if any {
                    continue;
                }
            }
        }
        items.push(RefItem { name, facts });
    }

    // Sorting: keys in the order given, the last one primary, refname as the final tie-break.
    struct Key {
        atom: String,
        descending: bool,
        version: bool,
    }
    let mut keys: Vec<Key> = Vec::new();
    for spec in args.sort.iter().rev() {
        let (descending, atom) = match spec.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, spec.as_str()),
        };
        let (version, atom) = match atom
            .strip_prefix("version:")
            .or_else(|| atom.strip_prefix("v:"))
        {
            Some(rest) => (true, rest),
            None => (false, atom),
        };
        if version && atom.is_empty() {
            return Ok(fatal("malformed field name: "));
        }
        if version && (atom.starts_with("version:") || atom.starts_with("v:")) {
            return Ok(fatal(format!("unknown field name: {atom}")));
        }
        keys.push(Key {
            atom: atom.to_string(),
            descending,
            version,
        });
    }
    let mut keyed: Vec<(Vec<SortValue>, RefItem)> = Vec::new();
    for item in items {
        let mut values = Vec::new();
        for key in &keys {
            let value = if key.version {
                atom_value(&key.atom, &item, &context).map(|v| SortValue::Text(v.text()))
            } else {
                sort_value(&key.atom, &item, &context)
            };
            match value {
                Ok(value) => values.push(value),
                Err(fatal) => return Ok(Err(fatal)),
            }
        }
        keyed.push((values, item));
    }
    let fold = |text: &str| -> String {
        if args.ignore_case {
            text.to_lowercase()
        } else {
            text.to_string()
        }
    };
    keyed.sort_by(|(left_values, left), (right_values, right)| {
        for (index, key) in keys.iter().enumerate() {
            let ordering = match (&left_values[index], &right_values[index]) {
                (SortValue::Text(a), SortValue::Text(b)) if key.version => versioncmp(a, b),
                (SortValue::Number(a), SortValue::Number(b)) => a.cmp(b),
                (SortValue::Text(a), SortValue::Text(b)) => fold(a).cmp(&fold(b)),
                _ => Ordering::Equal,
            };
            let ordering = if key.descending {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        fold(&left.name).cmp(&fold(&right.name))
    });
    if let Some(n) = args.count.filter(|n| *n > 0) {
        keyed.truncate(n);
    }

    let mut out = Vec::new();
    for (_, item) in &keyed {
        let line = match quote {
            Some(style) => evaluate_quoted(&nodes, item, &context, style),
            None => evaluate(&nodes, item, &context),
        };
        let line = match line {
            Ok(line) => line,
            Err(fatal) => return Ok(Err(fatal)),
        };
        if args.omit_empty && line.is_empty() {
            continue;
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    write_stdout(&out)?;
    Ok(Ok(0))
}
