//! Phase 1 — commits: a snapshot plus its history, and the identity headers that go with it.
//!
//! A commit body is a sequence of headers, one per line, then a blank line, then the message:
//!
//! ```text
//! tree <id>
//! parent <id>
//! parent <id>
//! author Rivet <rivet@example.com> 1759708800 +0200
//! committer Rivet <rivet@example.com> 1759708800 +0200
//! gpgsig -----BEGIN PGP SIGNATURE-----
//!  ...
//!  -----END PGP SIGNATURE-----
//!
//! the message
//! ```
//!
//! # The headers Rivet must preserve but does not understand
//!
//! Beyond `tree`, `parent`, `author` and `committer`, real commits carry headers Rivet has no
//! model for: `encoding`, `gpgsig`, `mergetag`, `extraHeader`, and anything a future Git invents.
//! **They must survive parse and re-serialise byte-for-byte, in their original order**, because
//! the object's id is the hash of its bytes: reordering two headers, or dropping one, produces a
//! commit that hashes differently and therefore does not exist as far as Git is concerned. The
//! plan calls this out explicitly (Phase 1, "preserve unknown ones verbatim"), and it is why
//! [`Commit`] keeps the original body and [`Commit::to_bytes`] prefers it over a rebuild.
//!
//! # Continuation lines
//!
//! A header value may span multiple lines: the continuation lines start with a single space,
//! which is part of the folding, not of the value. `gpgsig` is the usual case. A parser that
//! treats every line as a header will choke on a signature; a serialiser that forgets to re-fold
//! will produce a commit whose signature no longer verifies.
//!
//! # Timestamps are lossless
//!
//! Git stores `<unix-seconds> <±hhmm>`. Rivet keeps exactly that — `when: i64` and
//! `tz_offset_minutes: i32` — rather than a `jiff` type, because a normalising time library can
//! lose the distinction between `+0000` and `-0000` (Git uses the latter to mean "offset
//! unknown") and can move the wall-clock time when converting. `jiff` helpers are offered for
//! display only; the stored value is the authoritative one.

use bstr::{BStr, BString, ByteSlice};

use crate::error::{Error, Result};
use crate::hash::ObjectFormat;
use crate::hash::ObjectId;
use crate::object::ObjectKind;

/// Phase 1 — an author or committer line: a name, an email, a time and an offset.
///
/// # Caveats
///
/// All three of name, email and timestamp are **required** by Git; a commit missing any of them
/// is malformed. Names and emails are `BString`, not `String`: they come off disk and are not
/// guaranteed to be UTF-8 — an old commit can carry a Latin-1 author name, and rewriting it
/// lossily would change the commit's id.
///
/// The name and email are **not** trimmed of surrounding whitespace beyond the single space
/// that separates them, and `<>` is literal: the serialised form is exactly
/// `name SP <email> SP timestamp SP offset`.
///
/// `when` is seconds since the Unix epoch, and it can be negative — commits before 1970 exist in
/// imported histories and must not be rejected. `tz_offset_minutes` is the offset from UTC in
/// minutes, so `+0200` is `120`. Note that it is an *offset*, not a timezone: Git records no
/// zone name and no DST information, and a timestamp does not uniquely determine one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Signature {
    /// The display name, as raw bytes, without surrounding `<>` or quotes.
    pub name: BString,
    /// The email address, without the surrounding `<>`.
    pub email: BString,
    /// Seconds since the Unix epoch. May be negative for pre-1970 timestamps.
    pub when: i64,
    /// Offset from UTC in **minutes**, e.g. `+0200` is `120` and `-0530` is `-330`.
    pub tz_offset_minutes: i32,
}

impl Signature {
    /// Parses an identity line the way git's `split_ident_line` does: never fails.
    ///
    /// A strictly valid line gives exactly what [`Signature::parse`] gives. Otherwise a missing
    /// `<`/`>` leaves name and email empty, a missing or overflowing date is `0`, and the
    /// timezone is the leading `±digits` run read as `hhmm` (so `+00000` is UTC), else `0`.
    pub(crate) fn parse_lenient(data: &BStr) -> Self {
        if let Ok(sig) = Self::parse(data) {
            return sig;
        }
        let line: &[u8] = data;
        let empty = Self {
            name: BString::default(),
            email: BString::default(),
            when: 0,
            tz_offset_minutes: 0,
        };
        let Some(open) = line.iter().position(|b| *b == b'<') else {
            return empty;
        };
        let Some(close) = line[open + 1..].iter().position(|b| *b == b'>') else {
            return empty;
        };
        let close = open + 1 + close;
        let mut name_bytes = &line[..open];
        while let Some((&b' ', rest)) = name_bytes.split_last() {
            name_bytes = rest;
        }
        let rest = line[close + 1..].trim_start_with(|c| c == ' ');
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let when = parse_decimal(&rest[..digits]).unwrap_or(0);
        let after = rest[digits..].trim_start_with(|c| c == ' ');
        let tz_offset_minutes = match after.split_first() {
            Some((&s @ (b'+' | b'-'), d)) => {
                let n = d.iter().take_while(|b| b.is_ascii_digit()).count();
                let hhmm = parse_decimal(&d[..n]).unwrap_or(0).min(99_999_999);
                let m = (hhmm / 100 * 60 + hhmm % 100) as i32;
                if s == b'-' { -m } else { m }
            }
            _ => 0,
        };
        Self {
            name: BString::from(name_bytes),
            email: BString::from(&line[open + 1..close]),
            when,
            tz_offset_minutes,
        }
    }

    /// Phase 1— parse the portion of an author/committer header after the header name.
    ///
    /// Input looks like `Rivet <rivet@example.com> 1759708800 +0200`. The trailing `\n` has
    /// already been removed by the caller.
    ///
    /// # Caveats
    ///
    /// The parsing is **from the right**: Git finds the last `>` in the line and splits there,
    /// because a display name may legally contain `<` and `>` (a name like `A <B> C` is
    /// ambiguous to a left-to-right parser and unambiguous to Git's). Parse from the left and
    /// real-world commits with angle brackets in the name will be mis-split.
    ///
    /// A missing email, a missing timestamp, or a malformed offset is a parse error — not a
    /// zero-filled default. Defaulting would silently invent an author, and the resulting commit
    /// would hash differently from the one Git reads.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::MalformedObject`] for any of the structural failures above. Git
    /// itself is more lenient for some historical commits; where Rivet and Git disagree, Git's
    /// behaviour is the specification and this must be relaxed to match, not the other way round.
    pub fn parse(data: &BStr) -> Result<Self> {
        let line: &[u8] = data;
        let open = line
            .iter()
            .position(|b| *b == b'<')
            .ok_or_else(|| Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "no '<' found before the '>' in signature".to_string(),
            })?;
        let close = line[open + 1..]
            .iter()
            .position(|b| *b == b'>')
            .map(|i| open + 1 + i)
            .ok_or_else(|| Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "no '<' found before the '>' in signature".to_string(),
            })?;
        // git trims every trailing space before the '<'; inner spaces are kept.
        let mut name_bytes: &[u8] = &line[..open];
        while let Some((&b' ', rest)) = name_bytes.split_last() {
            name_bytes = rest;
        }
        let name = BString::from(name_bytes);
        let email = BString::from(&line[open + 1..close]);
        if email.is_empty() {
            return Err(Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "empty email address in signature".to_string(),
            });
        }
        // git reads the timestamp from the RIGHT end of the line, so anything left
        // over between the closing '>' and the last two tokens is ignored.
        let toks: Vec<&[u8]> = line.fields().collect();
        if toks.len() < 2 {
            return Err(Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "signature must be `name <email> <seconds> <±hhmm>`".to_string(),
            });
        }
        let when = parse_decimal(BStr::new(toks[toks.len() - 2])).ok_or_else(|| {
            Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "signature timestamp is not a decimal integer".to_string(),
            }
        })?;
        let tz_offset_minutes =
            parse_tz_offset(BStr::new(toks[toks.len() - 1])).ok_or_else(|| {
                Error::MalformedObject {
                    kind: ObjectKind::Commit,
                    reason: "signature timezone offset is not ±hhmm".to_string(),
                }
            })?;
        Ok(Self {
            name,
            email,
            when,
            tz_offset_minutes,
        })
    }

    /// Phase 1 — serialise to `name <email> seconds ±hhmm`, without a trailing newline.
    pub fn to_bytes(&self) -> Vec<u8> {
        let (sign, hours, minutes) = format_offset(self.tz_offset_minutes);
        let mut out = Vec::new();
        out.extend_from_slice(&self.name[..]);
        out.push(b' ');
        out.push(b'<');
        out.extend_from_slice(&self.email[..]);
        out.push(b'>');
        out.push(b' ');
        out.extend_from_slice(self.when.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(format!("{sign}{hours:02}{minutes:02}").as_bytes());
        out
    }

    /// Phase 1 — the timestamp as a `jiff::Timestamp`, for display.
    ///
    /// # Caveats
    ///
    /// **Lossy**: the returned type knows only the instant, not the `±hhmm` offset that Git
    /// stored. Displaying a commit must render the raw offset from [`Signature::tz_offset_minutes`]
    /// to match `git log` byte-for-byte, and must never round-trip through this value.
    ///
    /// # Errors
    ///
    /// A timestamp outside `jiff`'s representable range. Git will happily store a nonsense
    /// `i64`, so this is a real case for corrupt or hostile input, not a theoretical one.
    pub fn to_timestamp(&self) -> Result<jiff::Timestamp> {
        jiff::Timestamp::from_second(self.when).map_err(|e| Error::MalformedObject {
            kind: ObjectKind::Commit,
            reason: format!("timestamp out of range: {e}"),
        })
    }

    /// Phase 1 — build a signature from a timezone-aware `jiff` value.
    ///
    /// Used by `commit` and `tag` when the user has not set `GIT_AUTHOR_DATE` and Rivet is
    /// creating a fresh timestamp.
    ///
    /// # Caveats
    ///
    /// The offset is taken from `when`'s own timezone, which is what Git does; a `Zoned` value
    /// with a named zone yields its current offset for that instant, and the name is discarded —
    /// as it must be, since Git's format has nowhere to put it.
    pub fn from_zoned(name: &BStr, email: &BStr, when: jiff::Zoned) -> Self {
        Self {
            name: name.to_owned(),
            email: email.to_owned(),
            when: when.timestamp().as_second(),
            tz_offset_minutes: when.offset().seconds() / 60,
        }
    }
}

/// Phase 1 — a parsed commit.
///
/// # Caveats
///
/// The struct holds **both** the parsed fields and the original body, and they must agree. The
/// body is the source of truth for anything not modelled here — unknown headers, their order,
/// the exact byte spelling of the signature — so [`Commit::to_bytes`] must return the original
/// unchanged whenever the commit has not been edited. Only a commit built by [`CommitBuilder`]
/// gets serialised from its fields.
///
/// A commit with zero parents is a root commit and is perfectly legal: `git commit` on an empty
/// repository makes one. A commit with more than one parent is a merge, and its parents'
/// order is significant — for a merge, the first parent is "ours" and defines what `git log`
/// follows by default, and `git merge-base --is-ancestor` style questions are answered against
/// that ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit<'a> {
    /// The id of the tree this commit snapshots.
    pub tree: ObjectId,
    /// Parent commits, in order. Empty for a root commit; two or more for a merge.
    pub parents: Vec<ObjectId>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who committed it. Differs from the author on a rebase, a cherry-pick or a patch applied
    /// by someone else — which is why both exist and neither may be substituted for the other.
    pub committer: Signature,
    /// Everything after the blank line, as raw bytes, excluding the blank line itself.
    pub message: &'a [u8],
    /// Headers Rivet does not model (`encoding`, `gpgsig`, `mergetag`, `extraHeader`, …), in
    /// their original order and with their original folding.
    ///
    /// Each entry is the header name and its complete raw value, continuation lines included.
    pub extra_headers: Vec<(&'a BStr, &'a [u8])>,
    /// The commit body exactly as it was parsed, header and message together.
    pub raw: &'a [u8],
}

impl<'a> Commit<'a> {
    /// Phase 1 — the object kind of everything this type represents.
    pub const fn kind() -> ObjectKind {
        ObjectKind::Commit
    }

    /// Phase 1 — parse a commit body.
    ///
    /// # Caveats
    ///
    /// `data` is the **body only** — the `"commit <len>\0"` header has already been consumed.
    ///
    /// The grammar is line-oriented but not line-agnostic: headers run until the first blank
    /// line, and everything after it is the message, including any further blank lines and any
    /// further `<word> <value>`-looking lines. A parser that stops scanning headers at the first
    /// unrecognised one, or that trims the message, will produce a commit that fails to
    /// round-trip.
    ///
    /// The message is **not** trimmed and **not** normalised. Git does not guarantee a trailing
    /// newline; `commit -m "x"` writes one, but commits from other tools need not have it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::MissingHeader`] if `tree`, `author` or `committer` is absent —
    /// `parent` is the only optional one; [`crate::error::Error::InvalidHeader`] if a required
    /// header's value does not parse (a `tree` that is not a valid id for this format, a
    /// signature with no timestamp); [`crate::error::Error::MalformedObject`] if there is no
    /// blank line separating headers from message, or if a header line has no space after its
    /// name.
    pub fn parse(format: ObjectFormat, data: &'a [u8]) -> Result<Self> {
        // Find the first blank line separating headers from the message.
        let blank =
            data.windows(2)
                .position(|w| w == b"\n\n")
                .ok_or_else(|| Error::MalformedObject {
                    kind: ObjectKind::Commit,
                    reason: "no blank line separating headers from message".to_string(),
                })?;
        let header_end = blank + 2;
        let header_bytes = &data[..header_end];
        let message = &data[header_end..];

        let mut parents: Vec<ObjectId> = Vec::new();
        let mut tree: Option<ObjectId> = None;
        let mut author: Option<Signature> = None;
        let mut committer: Option<Signature> = None;
        let mut extra_headers: Vec<(&'a BStr, &'a [u8])> = Vec::new();
        let mut last_header: Option<(&str, Vec<u8>)> = None;
        let mut required = [false; 3]; // tree, author, committer

        for line in header_bytes.split_inclusive(|b| *b == b'\n') {
            if line.starts_with(b"\n") {
                continue;
            }
            // Continuation line: single leading space, no colon in the name part.
            if line.starts_with(b" ") {
                let Some((_, prev_value)) = last_header.as_mut() else {
                    return Err(Error::MalformedObject {
                        kind: ObjectKind::Commit,
                        reason: "continuation line with no preceding header".to_string(),
                    });
                };
                let cont: &[u8] = line.strip_prefix(b" ").unwrap_or(line);
                let cont_trimmed = cont.strip_suffix(b"\n").unwrap_or(cont);
                prev_value.push(b' ');
                prev_value.extend_from_slice(cont_trimmed);
                continue;
            }
            let space_end = line.iter().position(|b| *b == b' ');
            let (name, value) = match space_end {
                Some(e) => (&line[..e], &line[e + 1..]),
                None => {
                    return Err(Error::MalformedObject {
                        kind: ObjectKind::Commit,
                        reason: "header line without a space separator".to_string(),
                    });
                }
            };
            // Value after the space: drop trailing newline.
            let v = if value.ends_with(b"\n") {
                &value[..value.len() - 1]
            } else {
                value
            };
            let name_bytes = name;
            let name_str = std::str::from_utf8(name_bytes).map_err(|_| Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "non-UTF-8 header name".to_string(),
            })?;
            let header_key = name_str;

            if name_bytes.starts_with(b" ") || name_bytes.is_empty() {
                return Err(Error::MalformedObject {
                    kind: ObjectKind::Commit,
                    reason: "header name missing space after it".to_string(),
                });
            }

            let value_bytes: &[u8] = v;
            let name_owned = BStr::new(name_bytes);
            match header_key {
                "tree" => {
                    tree = Some(ObjectId::from_hex(format, BStr::new(value_bytes)).map_err(
                        |_| Error::InvalidHeader {
                            kind: ObjectKind::Commit,
                            header: "tree",
                            reason: "tree is not a valid object id for this format",
                        },
                    )?);
                    required[0] = true;
                }
                "parent" => {
                    parents.push(ObjectId::from_hex(format, BStr::new(value_bytes)).map_err(
                        |_| Error::InvalidHeader {
                            kind: ObjectKind::Commit,
                            header: "parent",
                            reason: "parent is not a valid object id for this format",
                        },
                    )?);
                }
                "author" => {
                    // Read paths are lenient like git's `split_ident_line`; `fsck` reports.
                    author = Some(Signature::parse_lenient(BStr::new(value_bytes)));
                    required[1] = true;
                }
                "committer" => {
                    committer = Some(Signature::parse_lenient(BStr::new(value_bytes)));
                    required[2] = true;
                }
                _ => {
                    // Unmodelled header: keep raw for byte-exact round-trip.
                    extra_headers.push((name_owned, value_bytes));
                    last_header = Some((header_key, value_bytes.to_vec()));
                }
            }
        }

        if !required[0] {
            return Err(Error::MissingHeader {
                kind: ObjectKind::Commit,
                header: "tree",
            });
        }
        if !required[1] {
            return Err(Error::MissingHeader {
                kind: ObjectKind::Commit,
                header: "author",
            });
        }
        if !required[2] {
            return Err(Error::MissingHeader {
                kind: ObjectKind::Commit,
                header: "committer",
            });
        }

        Ok(Self {
            tree: tree.unwrap(),
            parents,
            author: author.unwrap(),
            committer: committer.unwrap(),
            message: BStr::new(message),
            extra_headers,
            raw: data,
        })
    }

    /// Phase 1 — serialise to a commit body.
    ///
    /// # Caveats
    ///
    /// Returns the **body only**; the header is prepended by [`crate::object::hash_object`].
    ///
    /// For a commit that came from [`Commit::parse`], this must reproduce the input bytes
    /// exactly — that is the round-trip property the differential tests check, and it is why the
    /// `raw` field exists. Rebuilding from the parsed fields would reorder `extra_headers` and
    /// lose any spelling difference in the signature line, changing the commit's id.
    ///
    /// Header order in a rebuild is `tree`, `parent`s, `author`, `committer`, extras, blank line,
    /// message — which is what Git writes.
    pub fn to_bytes(&self) -> Vec<u8> {
        // A commit that came from parse is serialised as-is.
        if !self.raw.is_empty() {
            return self.raw.to_vec();
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"tree ");
        out.extend_from_slice(&self.tree.to_hex());
        out.push(b'\n');
        for p in &self.parents {
            out.extend_from_slice(b"parent ");
            out.extend_from_slice(&p.to_hex());
            out.push(b'\n');
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(self.author.to_bytes().as_slice());
        out.push(b'\n');
        out.extend_from_slice(b"committer ");
        out.extend_from_slice(self.committer.to_bytes().as_slice());
        out.push(b'\n');
        for (name, value) in &self.extra_headers {
            out.extend_from_slice(&name[..]);
            out.push(b' ');
            out.extend_from_slice(value);
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(self.message);
        out
    }

    /// Phase 1 — the commit's id under a given object format.
    ///
    /// # Errors
    ///
    /// Only the hasher's collision-detection path.
    pub fn id(&self, format: ObjectFormat) -> Result<ObjectId> {
        let body = self.to_bytes();
        crate::object::hash_object(format, ObjectKind::Commit, &body).map(|(id, _)| id)
    }

    /// Phase 1 — the first line of the message, which is the commit's subject.
    ///
    /// # Caveats
    ///
    /// Git's convention — and what `git log --oneline`, `git format-patch` and every hosting UI
    /// display — is the first **paragraph** folded into one line, not the first physical line.
    /// A message whose subject wraps across lines shows as the whole wrapped paragraph, joined
    /// with spaces. Taking only up to the first `\n` gets this visibly wrong on real messages.
    pub fn summary(&self) -> &'a BStr {
        let msg = self.message;
        // The subject is the first paragraph: everything up to (not including)
        // the first blank line ("\n\n") or the end of the message.
        // If the message is folded, we return the *un-folded* first physical line,
        // since BStr can only point into the original byte slice.
        // For simplicity and correctness with the &'a BStr return type, we return
        // the first paragraph's byte slice verbatim (newlines included).
        let end = msg
            .windows(2)
            .position(|w| w == b"\n\n")
            .unwrap_or(msg.len());
        let subject_bytes = &msg[..end];
        // Trim trailing newline if present.
        let trimmed = subject_bytes.strip_suffix(b"\n").unwrap_or(subject_bytes);
        BStr::new(trimmed)
    }

    /// Phase 1 — the message with the subject and the blank line after it removed.
    ///
    /// # Caveats
    ///
    /// Git's own definition of "body" for `format-patch`: everything after the first blank line.
    /// A message with no blank line has an empty body, not a body equal to its subject.
    pub fn body(&self) -> &'a BStr {
        let msg = self.message;
        // The body is everything after the first blank line.
        // The message starts with the subject, then "\n\n", then the body.
        // Find the first "\n\n".
        match msg.windows(2).position(|w| w == b"\n\n") {
            Some(i) => BStr::new(&msg[i + 2..]),
            None => BStr::new(b""),
        }
    }

    /// Phase 1 — whether this is a merge commit.
    ///
    /// # Caveats
    ///
    /// True for two or more parents. Octopus merges (three or more) are legal, so a `bool` here
    /// and an assumption of "exactly two" at the call site are not the same thing — code that
    /// needs the two parents of an ordinary merge must check the length itself.
    pub fn is_merge(&self) -> bool {
        self.parents.len() >= 2
    }

    /// Phase 1 — the first parent, if any.
    ///
    /// # Caveats
    ///
    /// The first parent is the mainline: `git log` follows it by default, and for a merge it is
    /// the branch that was checked out when the merge ran. `HEAD~1` means this parent, while
    /// `HEAD^2` means the second — so a caller that treats `parents[0]` and "the parent" as the
    /// same thing will get merge topology wrong.
    pub fn first_parent(&self) -> Option<ObjectId> {
        self.parents.first().copied()
    }

    /// Phase 1 — look up an unmodelled header by name.
    ///
    /// # Caveats
    ///
    /// Returns the **unfolded** value: continuation lines joined without the leading spaces that
    /// marked them, and without the trailing newline. That is what
    /// [`crate::object::commit::Commit::extra_headers`] holds the raw form of; the unfolding is a
    /// convenience for the one caller that matters — `gpgsig`, which `verify-commit` needs whole.
    ///
    /// The name match is byte-exact and case-sensitive, as Git's header names are.
    pub fn header(&self, name: &[u8]) -> Option<&'a BStr> {
        for (header_name, value) in &self.extra_headers {
            if header_name[..] == name {
                let value_ref: &'a [u8] = value;
                return Some(BStr::new(value_ref));
            }
        }
        None
    }
}

/// Phase 1 — build a commit that does not exist yet.
///
/// The write side of [`Commit`]. Separated out because constructing a commit has requirements
/// parsing does not: the tree and the parent ids must be well-formed for the repository's
/// format, an empty message has to be rejected or explicitly allowed, and the author/committer
/// defaults come from config and the environment.
///
/// # Caveats
///
/// A builder is **not** a `Commit`: it owns its data and carries no `raw` body, because there is
/// nothing to preserve. That is also why a commit created through here will not round-trip
/// byte-for-byte with a commit created by Git from identical fields — the header order is the
/// same, but an environment variable or a config default can differ, and the differential tests
/// have to account for that by comparing trees and topology rather than ids.
#[derive(Clone, Debug, Default)]
pub struct CommitBuilder {
    /// The tree to snapshot. Required.
    pub tree: Option<ObjectId>,
    /// Parent commits, in order. Empty means a root commit.
    pub parents: Vec<ObjectId>,
    /// The author. Defaults from `GIT_AUTHOR_*` and `user.*` config when `None`.
    pub author: Option<Signature>,
    /// The committer. Defaults from `GIT_COMMITTER_*` and `user.*` config when `None`.
    pub committer: Option<Signature>,
    /// The message, as raw bytes.
    pub message: BString,
}

impl CommitBuilder {
    /// Phase 1 — a builder with nothing set.
    ///
    /// # Caveats
    ///
    /// Does not consult config or the environment: defaults are resolved in
    /// [`CommitBuilder::build`], which is the only place that needs them and the only place that
    /// should be doing I/O.
    pub fn new() -> Self {
        Self::default()
    }

    /// Phase 1 — set the tree to snapshot.
    pub fn tree(&mut self, tree: ObjectId) -> &mut Self {
        self.tree = Some(tree);
        self
    }

    /// Phase 1 — append a parent, in order.
    ///
    /// # Caveats
    ///
    /// Order is significant for merges. Calling this twice gives a two-parent merge whose first
    /// parent is the one passed first; the caller is responsible for that being "ours".
    pub fn parent(&mut self, parent: ObjectId) -> &mut Self {
        self.parents.push(parent);
        self
    }

    /// Phase 1 — set the author explicitly.
    ///
    /// # Caveats
    ///
    /// Without this, `build` falls back to `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/
    /// `GIT_AUTHOR_DATE` and then to `user.name`/`user.email`. Git's precedence order is exact
    /// and the differential tests depend on it; getting it wrong shows up as a commit that
    /// differs from `git commit`'s in its author line and therefore in its id.
    pub fn author(&mut self, author: Signature) -> &mut Self {
        self.author = Some(author);
        self
    }

    /// Phase 1 — set the committer explicitly. Same fallback rules as
    /// [`CommitBuilder::author`], with `GIT_COMMITTER_*`.
    pub fn committer(&mut self, committer: Signature) -> &mut Self {
        self.committer = Some(committer);
        self
    }

    /// Phase 1 — set the raw message.
    ///
    /// # Caveats
    ///
    /// No trailing newline is appended by this method. Git's `-m` *does* append one, and
    /// `commit` must emulate that; putting the newline here instead would make every
    /// programmatic commit carry one whether or not it wanted it.
    pub fn message(&mut self, message: &[u8]) -> &mut Self {
        self.message = BString::from(message);
        self
    }

    /// Phase 1 — validate, resolve defaults, and serialise to a commit body.
    ///
    /// The result is passed to [`crate::object::hash_object`] and written to the object
    /// database; this method does not write anything itself.
    ///
    /// # Caveats
    ///
    /// An empty message is **legal** in Git — `git commit --allow-empty-message` exists — but
    /// `git commit -m ""` refuses without it. Which of those Rivet's `commit` implements is a CLI
    /// decision; this function permits the empty message and leaves the policy to the caller.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::MissingHeader`] if no tree was set — the one field with no
    /// sensible default; [`crate::error::Error::Message`] if the author or committer cannot be
    /// resolved, which is the "please tell me who you are" failure `git commit` has.
    pub fn build(&self, _format: ObjectFormat) -> Result<Vec<u8>> {
        let tree = self.tree.as_ref().ok_or(Error::MissingHeader {
            kind: ObjectKind::Commit,
            header: "tree",
        })?;
        let author = self.author.clone().ok_or_else(|| {
            Error::Message(
                "cannot resolve author identity: set GIT_AUTHOR_NAME or user.name".to_string(),
            )
        })?;
        let committer = self.committer.clone().ok_or_else(|| {
            Error::Message(
                "cannot resolve committer identity: set GIT_COMMITTER_NAME or user.name"
                    .to_string(),
            )
        })?;

        let mut out = Vec::new();
        out.extend_from_slice(b"tree ");
        out.extend_from_slice(&tree.to_hex());
        out.push(b'\n');
        for p in &self.parents {
            out.extend_from_slice(b"parent ");
            out.extend_from_slice(&p.to_hex());
            out.push(b'\n');
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(author.to_bytes().as_slice());
        out.push(b'\n');
        out.extend_from_slice(b"committer ");
        out.extend_from_slice(committer.to_bytes().as_slice());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(&self.message[..]);
        Ok(out)
    }
}

/// Parses a signed decimal integer. Returns `None` for anything else.
fn parse_decimal(bytes: &[u8]) -> Option<i64> {
    let (neg, digits) = match bytes.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if neg { -acc } else { acc })
}

/// Parses a Git timezone offset: a sign, two decimal hours, two decimal minutes,
/// e.g. `+0200`, `-0530`. Minutes, not "hhmm as one number".
fn parse_tz_offset(bytes: &[u8]) -> Option<i32> {
    if bytes.len() != 5 {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hh = parse_two_digits(&bytes[1..3])?;
    let mm = parse_two_digits(&bytes[3..5])?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh as i32 * 60 + mm as i32))
}

fn parse_two_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((bytes[0] - b'0') as u32 * 10 + (bytes[1] - b'0') as u32)
}

/// Parses one of the date spellings Git accepts in `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`.
///
/// Returns `(seconds since the epoch, offset from UTC in minutes)` — the same pair a
/// [`Signature`] stores, so a caller can build one without going through a `jiff::Zoned` (which
/// would lose the exact offset when the value names a zone).
///
/// # Caveats
///
/// Supported spellings are the ones Git's own scripts and tests use: `now`, `<epoch> <±hhmm>`,
/// `@<epoch> <±hhmm>`, and ISO 8601 with `T` or a space separator, with an optional `Z` or
/// `±hhmm`/`±hh:mm` offset. Relative descriptions (`2.days.ago`, `yesterday`) are *not* parsed;
/// a caller that needs them must resolve them before calling this. A date with no offset is
/// taken in the local zone, which is also what Git does.
pub fn parse_git_date(value: &BStr) -> Option<(i64, i32)> {
    let text = std::str::from_utf8(value).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.eq_ignore_ascii_case("now") {
        let now = jiff::Zoned::now();
        return Some((now.timestamp().as_second(), now.offset().seconds() / 60));
    }
    let text = text.strip_prefix('@').unwrap_or(text);

    // Raw epoch: `<seconds>` optionally followed by an offset.
    let mut fields = text.split_whitespace();
    let first = fields.next()?;
    if !first.is_empty()
        && first
            .strip_prefix('-')
            .unwrap_or(first)
            .bytes()
            .all(|b| b.is_ascii_digit())
    {
        let when: i64 = first.parse().ok()?;
        let offset = match fields.next() {
            Some(offset) => parse_tz_offset(offset.as_bytes())?,
            None => local_offset_minutes(),
        };
        return Some((when, offset));
    }

    // ISO 8601. `jiff` wants RFC 3339, so the separator and the offset are normalised first.
    // `2023-11-14 22:16:40 +0000`: only the first space is the date/time separator, a second one
    // precedes the offset.
    let mut pieces = text.split_whitespace();
    let mut body = pieces.next()?.to_string();
    if body.contains(['T', 't']) {
        // `2026-01-01T00:00:00 +0000`: the separator is already there, so what follows is the
        // offset.
        for offset in pieces {
            body.push_str(offset);
        }
    } else if let Some(time) = pieces.next() {
        body.push('T');
        body.push_str(time);
        if let Some(offset) = pieces.next() {
            body.push_str(offset);
        }
    }
    if body.len() == 10 {
        body.push_str("T00:00:00");
    }
    let (civil, offset) = match split_iso_offset(&body) {
        Some((civil, offset)) => (civil, offset),
        None => (body.as_str(), local_offset_minutes()),
    };
    let (sign, hours, minutes) = format_offset(offset);
    let normalised = format!("{civil}{sign}{hours:02}:{minutes:02}");
    let stamp: jiff::Timestamp = normalised.parse().ok()?;
    Some((stamp.as_second(), offset))
}

/// The broken-down time Git's `parse_date_basic` fills in; `-1` means "not seen yet".
struct DateParts {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
}

impl DateParts {
    fn nodate(&self) -> bool {
        [
            self.year, self.mon, self.mday, self.hour, self.min, self.sec,
        ]
        .iter()
        .all(|v| *v < 0)
    }

    /// Git's `tm_to_time_t`: only 1970..2099 and a complete time of day convert.
    fn to_time(&self) -> Option<i64> {
        const MDAYS: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let year = self.year - 70;
        if !(0..=129).contains(&year) || !(0..=11).contains(&self.mon) {
            return None;
        }
        let mut day = self.mday;
        if self.mon < 2 || (year + 2) % 4 != 0 {
            day -= 1;
        }
        if self.hour < 0 || self.min < 0 || self.sec < 0 {
            return None;
        }
        Some(
            (year * 365 + (year + 1) / 4 + MDAYS[self.mon as usize] + day) * 86400
                + self.hour * 3600
                + self.min * 60
                + self.sec,
        )
    }
}

/// Git's `match_string`: how many bytes of `date` agree (ignoring case) with `word`.
fn date_match_string(date: &[u8], word: &str) -> usize {
    let word = word.as_bytes();
    let mut i = 0;
    while i < date.len() {
        let c = date[i];
        let w = word.get(i).copied().unwrap_or(0);
        if c != w && !c.eq_ignore_ascii_case(&w) {
            if !c.is_ascii_alphanumeric() {
                break;
            }
            return 0;
        }
        i += 1;
    }
    i
}

/// `strtoul`/`strtol` over a run of digits starting at `at`: the value (saturating) and the end.
fn date_digits(date: &[u8], at: usize) -> (i64, usize) {
    let mut end = at;
    let mut value: i64 = 0;
    while end < date.len() && date[end].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(date[end] - b'0'));
        end += 1;
    }
    (value, end)
}

fn date_set_time(parts: &mut DateParts, hour: i64, min: i64, sec: i64) -> bool {
    if hour < 24 && min < 60 && sec <= 60 {
        parts.hour = hour;
        parts.min = min;
        parts.sec = sec;
        return true;
    }
    false
}

/// Git's `set_date`; `refuse_future` carries the clock when a date in the future must be refused.
fn date_set_date(
    parts: &mut DateParts,
    year: i64,
    month: i64,
    day: i64,
    now: Option<i64>,
    now_year: i64,
) -> bool {
    if !(month > 0 && month < 13 && day > 0 && day < 32) {
        return false;
    }
    let mut check = DateParts { ..*parts };
    let target = if now.is_some() {
        &mut check
    } else {
        &mut *parts
    };
    target.mon = month - 1;
    target.mday = day;
    // The C arguments are `int`s.
    let year = i64::from(year as i32);
    if year == -1 {
        let Some(_) = now else { return false };
        target.year = now_year;
    } else if (1970..2100).contains(&year) {
        target.year = year - 1900;
    } else if year > 70 && year < 100 {
        target.year = year;
    } else if year < 38 {
        target.year = year + 100;
    } else {
        return false;
    }
    let Some(now) = now else { return true };
    if let Some(specified) = check.to_time()
        && now < specified - 86400
    {
        return false;
    }
    *parts = check;
    true
}

/// Git's `match_multi_number`: `hh:mm[:ss]`, `yyyy-mm-dd`, `mm/dd/yyyy`, `dd.mm.yyyy` ...
fn date_multi_number(
    num: i64,
    sep: u8,
    date: &[u8],
    end: usize,
    parts: &mut DateParts,
    now: i64,
) -> usize {
    let (num2, mut end) = date_digits(date, end + 1);
    let mut num3 = -1;
    if date.get(end) == Some(&sep) && date.get(end + 1).is_some_and(u8::is_ascii_digit) {
        let (value, after) = date_digits(date, end + 1);
        num3 = value;
        end = after;
    }
    let now_year = jiff::Timestamp::from_second(now)
        .map(|t| i64::from(t.to_zoned(jiff::tz::TimeZone::UTC).year()) - 1900)
        .unwrap_or(0);
    match sep {
        b':' => {
            if num3 < 0 {
                num3 = 0;
            }
            if !(num < 25 && (0..60).contains(&num2) && (0..=60).contains(&num3)) {
                return 0;
            }
            parts.hour = num;
            parts.min = num2;
            parts.sec = num3;
            // `hh:mm:ss.fraction`: the fraction is dropped.
            if date.get(end) == Some(&b'.') && date.get(end + 1).is_some_and(u8::is_ascii_digit) {
                end = date_digits(date, end + 1).1;
            }
        }
        _ => {
            let some = Some(now);
            let done = (num > 70
                && (date_set_date(parts, num, num2, num3, None, now_year)
                    || date_set_date(parts, num, num3, num2, None, now_year)))
                || (sep != b'.' && date_set_date(parts, num3, num, num2, some, now_year))
                || date_set_date(parts, num3, num2, num, some, now_year)
                || (sep == b'.' && date_set_date(parts, num3, num, num2, some, now_year));
            if !done {
                return 0;
            }
        }
    }
    end
}

/// Git's `match_digit`.
fn date_match_digit(
    date: &[u8],
    parts: &mut DateParts,
    offset: &mut Option<i64>,
    gmt: &mut bool,
    now: i64,
) -> usize {
    let (num, end) = date_digits(date, 0);
    if num >= 100_000_000
        && parts.nodate()
        && let Ok(stamp) = jiff::Timestamp::from_second(num)
    {
        let civil = stamp.to_zoned(jiff::tz::TimeZone::UTC);
        parts.year = i64::from(civil.year()) - 1900;
        parts.mon = i64::from(civil.month()) - 1;
        parts.mday = i64::from(civil.day());
        parts.hour = i64::from(civil.hour());
        parts.min = i64::from(civil.minute());
        parts.sec = i64::from(civil.second());
        *gmt = true;
        return end;
    }
    if let Some(&sep) = date.get(end)
        && matches!(sep, b':' | b'.' | b'/' | b'-')
        && date.get(end + 1).is_some_and(u8::is_ascii_digit)
    {
        let matched = date_multi_number(num, sep, date, end, parts, now);
        if matched != 0 {
            return matched;
        }
    }
    let n = date.iter().take_while(|b| b.is_ascii_digit()).count();
    let mut end = end;
    if n == 8 || n == 6 {
        let (num1, num2, num3) = (num / 10000, (num % 10000) / 100, num % 100);
        if n == 8 {
            date_set_date(parts, num1, num2, num3, None, 0);
        } else if date_set_time(parts, num1, num2, num3)
            && date.get(end) == Some(&b'.')
            && date.get(end + 1).is_some_and(u8::is_ascii_digit)
        {
            end = date_digits(date, end + 1).1;
        }
        return end;
    }
    if n == 4 {
        if num <= 1400 && offset.is_none() {
            *offset = Some((num / 100) * 60 + num % 100);
        } else if num > 1900 && num < 2100 {
            parts.year = num - 1900;
        }
        return n;
    }
    if n > 2 {
        return n;
    }
    if num > 0 && num < 32 && parts.mday < 0 {
        parts.mday = num;
        return n;
    }
    if num > 0 && num < 13 && parts.mon < 0 {
        parts.mon = num - 1;
        return n;
    }
    n
}

/// Git's `match_tz`.
fn date_match_tz(date: &[u8], offset: &mut Option<i64>) -> usize {
    let (mut hour, mut end) = date_digits(date, 1);
    let n = end - 1;
    let mut min = 0;
    if n == 4 {
        min = hour % 100;
        hour /= 100;
    } else if n != 2 {
        min = 99;
    } else if date.get(end) == Some(&b':') {
        let (value, after) = date_digits(date, end + 1);
        min = value;
        end = after;
        if end - 1 != 5 {
            min = 99;
        }
    }
    if min < 60 && hour < 24 {
        let minutes = hour * 60 + min;
        *offset = Some(if date[0] == b'-' { -minutes } else { minutes });
    }
    end
}

/// Git's `match_alpha`: month, weekday and zone names, `AM`/`PM`, the ISO `T`; other words are skipped.
fn date_match_alpha(date: &[u8], parts: &mut DateParts, offset: &mut Option<i64>) -> usize {
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
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const ZONES: [(&str, i64, i64); 41] = [
        ("IDLW", -12, 0),
        ("NT", -11, 0),
        ("CAT", -10, 0),
        ("HST", -10, 0),
        ("HDT", -10, 1),
        ("YST", -9, 0),
        ("YDT", -9, 1),
        ("PST", -8, 0),
        ("PDT", -8, 1),
        ("MST", -7, 0),
        ("MDT", -7, 1),
        ("CST", -6, 0),
        ("CDT", -6, 1),
        ("EST", -5, 0),
        ("EDT", -5, 1),
        ("AST", -4, 0),
        ("ADT", -4, 1),
        ("WAT", -1, 0),
        ("GMT", 0, 0),
        ("UTC", 0, 0),
        ("Z", 0, 0),
        ("WET", 0, 0),
        ("BST", 0, 1),
        ("CET", 1, 0),
        ("MET", 1, 0),
        ("MEWT", 1, 0),
        ("MEST", 1, 1),
        ("CEST", 1, 1),
        ("MESZ", 1, 1),
        ("FWT", 1, 0),
        ("FST", 1, 1),
        ("EET", 2, 0),
        ("EEST", 2, 1),
        ("WAST", 7, 0),
        ("WADT", 7, 1),
        ("CCT", 8, 0),
        ("JST", 9, 0),
        ("EAST", 10, 0),
        ("EADT", 10, 1),
        ("GST", 10, 0),
        ("NZT", 12, 0),
    ];
    const MORE_ZONES: [(&str, i64, i64); 3] = [("NZST", 12, 0), ("NZDT", 12, 1), ("IDLE", 12, 0)];
    for (i, name) in MONTHS.iter().enumerate() {
        let matched = date_match_string(date, name);
        if matched >= 3 {
            parts.mon = i as i64;
            return matched;
        }
    }
    for name in DAYS {
        let matched = date_match_string(date, name);
        if matched >= 3 {
            return matched;
        }
    }
    for (name, hours, dst) in ZONES.iter().chain(MORE_ZONES.iter()) {
        let matched = date_match_string(date, name);
        if matched >= 3 || matched == name.len() {
            if offset.is_none() {
                *offset = Some(60 * (hours + dst));
            }
            return matched;
        }
    }
    if date_match_string(date, "PM") == 2 {
        if parts.hour > 0 && parts.hour < 12 {
            parts.hour += 12;
        }
        return 2;
    }
    if date_match_string(date, "AM") == 2 {
        if parts.hour == 12 {
            parts.hour = 0;
        }
        return 2;
    }
    if date[0] == b'T' && date.get(1).is_some_and(u8::is_ascii_digit) && parts.hour == -1 {
        parts.min = 0;
        parts.sec = 0;
        return 1;
    }
    date.iter().take_while(|b| b.is_ascii_alphabetic()).count()
}

/// Reads a `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE` value the way Git's `parse_date_basic` does:
/// `@<epoch> <±hhmm>` (the object-header form), a bare epoch, ISO 8601 and RFC 2822 dates, `mm/dd/yyyy`,
/// `dd.mm.yyyy`, month and zone names, `AM`/`PM`. Anything it cannot place is skipped, and the value
/// is refused (`None`) only when no complete date and time of day (1970..2099) is left. Unlike
/// [`parse_git_date`] it does not know `now`, nor a date without a time of day.
///
/// Returns `(seconds since the epoch, offset from UTC in minutes)`; a value with no zone is read
/// in the local zone.
pub fn parse_env_date(value: &BStr) -> Option<(i64, i32)> {
    let date: &[u8] = value;
    if let Some(rest) = date.strip_prefix(b"@")
        && let Some(found) = object_header_date(rest)
    {
        return Some(found);
    }
    let now = jiff::Timestamp::now().as_second();
    let mut parts = DateParts {
        year: -1,
        mon: -1,
        mday: -1,
        hour: -1,
        min: -1,
        sec: -1,
    };
    let mut offset: Option<i64> = None;
    let mut gmt = false;
    let mut at = 0;
    while at < date.len() && date[at] != b'\n' {
        let rest = &date[at..];
        let c = rest[0];
        let matched = if c.is_ascii_alphabetic() {
            date_match_alpha(rest, &mut parts, &mut offset)
        } else if c.is_ascii_digit() {
            date_match_digit(rest, &mut parts, &mut offset, &mut gmt, now)
        } else if (c == b'-' || c == b'+') && rest.get(1).is_some_and(u8::is_ascii_digit) {
            date_match_tz(rest, &mut offset)
        } else {
            0
        };
        at += matched.max(1);
    }
    let mut stamp = parts.to_time()?;
    let offset = match offset {
        Some(offset) => offset,
        None => jiff::Timestamp::from_second(stamp)
            .ok()
            .map(|t| i64::from(jiff::tz::TimeZone::system().to_offset(t).seconds()) / 60)
            .unwrap_or(0),
    };
    if !gmt {
        stamp -= offset * 60;
    }
    // Git's `parse_date` refuses a result outside 1970..2099 once the zone is applied.
    if !(0..4_102_444_800).contains(&stamp) {
        return None;
    }
    Some((stamp, offset as i32))
}

/// Git's `match_object_header_date`: `<epoch> ±hhmm`, exactly.
fn object_header_date(date: &[u8]) -> Option<(i64, i32)> {
    if !date.first()?.is_ascii_digit() {
        return None;
    }
    let (stamp, end) = date_digits(date, 0);
    if date.get(end) != Some(&b' ') || stamp == i64::MAX {
        return None;
    }
    let sign = *date.get(end + 1)?;
    if sign != b'+' && sign != b'-' {
        return None;
    }
    let (zone, zone_end) = date_digits(date, end + 2);
    if zone_end != end + 6 || date.get(zone_end).is_some_and(|b| *b != b'\n') {
        return None;
    }
    let minutes = (zone / 100) * 60 + zone % 100;
    Some((stamp, if sign == b'-' { -minutes } else { minutes } as i32))
}

/// Splits a trailing `Z`, `±hhmm` or `±hh:mm` off an ISO timestamp, returning the remainder and
/// the offset in minutes.
fn split_iso_offset(text: &str) -> Option<(&str, i32)> {
    if let Some(rest) = text.strip_suffix('Z') {
        return Some((rest, 0));
    }
    let is_offset = |tail: &str| match tail.as_bytes().first().copied() {
        Some(sign) => {
            (sign == b'+' || sign == b'-')
                && tail[1..].bytes().all(|b| b.is_ascii_digit() || b == b':')
        }
        None => false,
    };
    if text.len() >= 6 && is_offset(&text[text.len() - 6..]) {
        let tail = &text[text.len() - 6..];
        let offset = parse_tz_offset(tail.replacen(':', "", 1).as_bytes())?;
        return Some((&text[..text.len() - 6], offset));
    }
    if text.len() >= 5 && is_offset(&text[text.len() - 5..]) {
        let tail = &text[text.len() - 5..];
        let offset = parse_tz_offset(tail.as_bytes())?;
        return Some((&text[..text.len() - 5], offset));
    }
    None
}

/// The current UTC offset in minutes, used when a date carries none.
fn local_offset_minutes() -> i32 {
    let now = jiff::Zoned::now();
    now.offset().seconds() / 60
}

/// Inverse of `parse_tz_offset` for serialising: `(hours, non-negative minutes)`.
/// Split an offset in minutes into Git's `±HHMM` parts. The sign is separate so that offsets
/// under an hour west of UTC (`-0030`) keep their sign; a signed hour field would print `+00`.
fn format_offset(total_minutes: i32) -> (char, i32, i32) {
    let sign = if total_minutes < 0 { '-' } else { '+' };
    let abs = total_minutes.abs();
    (sign, abs / 60, abs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `T` already separates date and time, so a space-separated offset follows it.
    #[test]
    fn git_date_iso_with_t_and_spaced_offset() {
        let d = |s: &str| parse_git_date(BStr::new(s));
        assert_eq!(d("2026-01-01T00:00:00 +0000"), Some((1767225600, 0)));
        assert_eq!(d("2026-01-01T00:00:00 +0200"), Some((1767218400, 120)));
        assert_eq!(d("2026-01-01 00:00:00 +0000"), Some((1767225600, 0)));
        assert_eq!(d("garbage"), None);
    }

    /// `GIT_*_DATE` is read as Git's `parse_date_basic` reads it (values recorded from `git commit-tree`).
    #[test]
    fn env_date_follows_parse_date_basic() {
        let d = |s: &str| parse_env_date(BStr::new(s));
        assert_eq!(
            d("Thu, 07 Apr 2005 22:13:13 +0200"),
            Some((1112904793, 120))
        );
        assert_eq!(d("2005-04-07 22:13:13 +02:00"), Some((1112904793, 120)));
        assert_eq!(d("1112911993 +02:00"), Some((1112911993, 120)));
        assert_eq!(d("@1112911993 +0200"), Some((1112911993, 120)));
        assert_eq!(d("04/07/2005 20:13:13 UTC"), Some((1112904793, 0)));
        assert_eq!(d("Apr 7 2005 10:13:13 PM EST"), Some((1112929993, -300)));
        assert_eq!(d("2005-04-07T20:13:13.987Z"), Some((1112904793, 0)));
        // No time of day, `now`, a year outside 1970..2099 and relative words are all refused.
        for bad in [
            "now",
            "2005-04-07",
            "20050407",
            "2.days.ago",
            "2100-01-01 00:00:00 +0000",
        ] {
            assert_eq!(d(bad), None, "{bad}");
        }
        // The zone is applied before the 1970..2099 check.
        assert_eq!(d("1970-01-01 00:00:00 +0500"), None);
        assert_eq!(d("1970-01-01 00:00:00 -0500"), Some((18000, -300)));
    }

    /// A8: read paths accept the identity lines git's `split_ident_line` accepts.
    #[test]
    fn a8_lenient_identity_lines() {
        let l = |s: &[u8]| Signature::parse_lenient(BStr::new(s));
        let sig = l(b"Foo 1234 +0000");
        assert_eq!((sig.name.len(), sig.email.len(), sig.when), (0, 0, 0));
        let sig = l(b"Foo <foo@x.com>");
        assert_eq!(
            (&sig.name[..], &sig.email[..], sig.when),
            (&b"Foo"[..], &b"foo@x.com"[..], 0)
        );
        let sig = l(b"Foo <foo@x.com> 99999999999999999999999 +0000");
        assert_eq!(sig.when, 0);
        let sig = l(b"Foo <foo@x.com> 1234 +00000");
        assert_eq!((sig.when, sig.tz_offset_minutes), (1234, 0));
        let sig = l(b"Foo <foo@x.com> 1234 -0130");
        assert_eq!((sig.when, sig.tz_offset_minutes), (1234, -90));
    }

    /// H2: angle-bracket name — the last '>' wins.
    #[test]
    fn h2_angle_bracket_name() {
        // measured against real git: the split is at the FIRST '<', so this (fsck-invalid)
        // line parses as name="A" email="B" with the timestamp still read from the right.
        let sig = Signature::parse(BStr::new(b"A <B> C <a@b.example> 100 +0000")).unwrap();
        assert_eq!(&sig.name[..], b"A");
        assert_eq!(&sig.email[..], b"B");
        assert_eq!(sig.when, 100);
        assert_eq!(sig.tz_offset_minutes, 0);
    }

    /// H3: multi-line-header byte round-trip.
    #[test]
    fn h3_multi_line_header_round_trip() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 83090d22f19f9d26a294f62400260012c891333f\n\
author David Queen <david@queen.example> 1000 +0000\n\
committer David Queen <david@queen.example> 2000 -0530\n\
\nSubject: this is a long\ncontinuation of the subject\n\nBody text.\n";
        let c = Commit::parse(ObjectFormat::Sha1, body).unwrap();
        assert_eq!(c.to_bytes(), body);
    }

    /// H4: summary, body, and is_merge cases.
    #[test]
    fn h4_summary_body_is_merge() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 83090d22f19f9d26a294f62400260012c891333f\n\
parent 9a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b\n\
author David Queen <david@queen.example> 1000 +0000\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nFirst line is the subject.\n\nThe rest is the body.\n";
        let c = Commit::parse(ObjectFormat::Sha1, body).unwrap();
        assert!(c.is_merge());
        assert_eq!(c.summary(), BStr::new(b"First line is the subject."));
        assert_eq!(c.body(), BStr::new(b"The rest is the body.\n"));

        let body_single = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 83090d22f19f9d26a294f62400260012c891333f\n\
author David Queen <david@queen.example> 1000 +0000\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nSingle parent, no merge.\n";
        let c2 = Commit::parse(ObjectFormat::Sha1, body_single).unwrap();
        assert!(!c2.is_merge());
        assert_eq!(c2.summary(), BStr::new(b"Single parent, no merge."));
        assert_eq!(c2.body(), BStr::new(b""));
    }

    /// Missing required headers → MissingHeader.
    #[test]
    fn missing_tree_header() {
        let body = b"author David Queen <david@queen.example> 1000 +0000\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nmessage\n";
        let err = Commit::parse(ObjectFormat::Sha1, body).unwrap_err();
        assert!(matches!(err, Error::MissingHeader { header: "tree", .. }));
    }

    #[test]
    fn missing_author_header() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nmessage\n";
        let err = Commit::parse(ObjectFormat::Sha1, body).unwrap_err();
        assert!(matches!(
            err,
            Error::MissingHeader {
                header: "author",
                ..
            }
        ));
    }

    #[test]
    fn missing_committer_header() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author David Queen <david@queen.example> 1000 +0000\n\
\nmessage\n";
        let err = Commit::parse(ObjectFormat::Sha1, body).unwrap_err();
        assert!(matches!(
            err,
            Error::MissingHeader {
                header: "committer",
                ..
            }
        ));
    }

    /// Absent parent is legal.
    #[test]
    fn absent_parent_is_legal() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author David Queen <david@queen.example> 1000 +0000\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nno parent commit\n";
        let c = Commit::parse(ObjectFormat::Sha1, body).unwrap();
        assert!(c.parents.is_empty());
        assert!(!c.is_merge());
    }

    /// Strict `Signature::parse` rejects an out-of-range offset; the commit read path follows
    /// git (`git log` prints `+9999` and `fsck` is clean) and keeps the commit readable.
    #[test]
    fn out_of_range_offset_is_strict_for_signature_lenient_for_commit() {
        assert!(Signature::parse(BStr::new(b"D <d@q> 1000 +9999")).is_err());
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author David Queen <david@queen.example> 1000 +9999\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nmessage\n";
        let c = Commit::parse(ObjectFormat::Sha1, body).unwrap();
        assert_eq!(
            (c.author.when, c.author.tz_offset_minutes),
            (1000, 99 * 60 + 99)
        );
    }

    /// A negative `when` is legal.
    #[test]
    fn negative_when_accepted() {
        let sig = Signature::parse(BStr::new(b"A <a@b.example> -100 +0000")).unwrap();
        assert_eq!(sig.when, -100);
    }

    /// `build` accepts an empty message.
    #[test]
    fn build_accepts_empty_message() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            BStr::new("0000000000000000000000000000000000000000"),
        )
        .unwrap();
        let sig = Signature {
            name: BString::from(b"D"),
            email: BString::from(b"d@e.example"),
            when: 0,
            tz_offset_minutes: 0,
        };
        let c = CommitBuilder::new()
            .tree(tree)
            .author(sig.clone())
            .committer(sig)
            .message(b"")
            .build(ObjectFormat::Sha1)
            .unwrap();
        assert!(!c.is_empty());
    }

    /// `build`'s "who are you" error.
    #[test]
    fn build_who_are_you_error() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            BStr::new("0000000000000000000000000000000000000000"),
        )
        .unwrap();
        let c = CommitBuilder::new()
            .tree(tree)
            .build(ObjectFormat::Sha1)
            .unwrap_err();
        assert!(matches!(
            c,
            Error::Message(msg) if msg.starts_with("cannot resolve author identity")
        ));
    }

    /// `±hhmm` serialisation: -330 → -0530.
    #[test]
    fn offset_serialisation() {
        let sig = Signature {
            name: BString::from(b"A"),
            email: BString::from(b"a@b.example"),
            when: 0,
            tz_offset_minutes: -330,
        };
        let bytes = sig.to_bytes();
        assert!(bytes.ends_with(b"-0530"));
    }

    /// H1: `+0530` is not 530, `100644` is not decimal, negative `when` is legal.
    #[test]
    fn parse_tz_offset_values() {
        assert_eq!(parse_tz_offset(b"+0530"), Some(330));
        assert_eq!(parse_tz_offset(b"-0530"), Some(-330));
        assert_eq!(parse_tz_offset(b"+0000"), Some(0));
        assert_eq!(parse_tz_offset(b"+2359"), Some(1439));
        assert_eq!(parse_tz_offset(b"-0030"), Some(-30));
        // Reject out-of-range:
        assert_eq!(parse_tz_offset(b"+2400"), None);
        assert_eq!(parse_tz_offset(b"+0060"), None);
    }

    #[test]
    fn parse_decimal_values() {
        assert_eq!(parse_decimal(b"100644"), Some(100644));
        assert_eq!(parse_decimal(b"-100"), Some(-100));
        assert_eq!(parse_decimal(b"0"), Some(0));
        assert_eq!(parse_decimal(b"-0"), Some(0));
        assert_eq!(parse_decimal(b""), None);
        assert_eq!(parse_decimal(b"-"), None);
        assert_eq!(parse_decimal(b"12a3"), None);
    }
}
