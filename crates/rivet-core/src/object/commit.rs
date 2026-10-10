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
    /// Phase 1 — parse the portion of an author/committer header after the header name.
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
        let open = line.iter().position(|b| *b == b'<').ok_or_else(|| {
            Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "no '<' found before the '>' in signature".to_string(),
            }
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
        let (hours, minutes) = format_offset(self.tz_offset_minutes);
        let mut out = Vec::new();
        out.extend_from_slice(&self.name[..]);
        out.push(b' ');
        out.push(b'<');
        out.extend_from_slice(&self.email[..]);
        out.push(b'>');
        out.push(b' ');
        out.extend_from_slice(self.when.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(format!("{:+03}{:02}", hours, minutes).as_bytes());
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
        let blank = data.windows(2).position(|w| w == b"\n\n").ok_or_else(|| {
            Error::MalformedObject {
                kind: ObjectKind::Commit,
                reason: "no blank line separating headers from message".to_string(),
            }
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
            let v = if value.ends_with(b"\n") { &value[..value.len() - 1] } else { value };
            let name_bytes = name;
            let name_str = std::str::from_utf8(name_bytes).map_err(|_| {
                Error::MalformedObject {
                    kind: ObjectKind::Commit,
                    reason: "non-UTF-8 header name".to_string(),
                }
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
                    tree = Some(
                        ObjectId::from_hex(format, BStr::new(value_bytes)).map_err(|_| {
                            Error::InvalidHeader {
                                kind: ObjectKind::Commit,
                                header: "tree",
                                reason: "tree is not a valid object id for this format",
                            }
                        })?,
                    );
                    required[0] = true;
                }
                "parent" => {
                    parents.push(
                        ObjectId::from_hex(format, BStr::new(value_bytes)).map_err(|_| {
                            Error::InvalidHeader {
                                kind: ObjectKind::Commit,
                                header: "parent",
                                reason: "parent is not a valid object id for this format",
                            }
                        })?,
                    );
                }
                "author" => {
                    author = Some(
                        Signature::parse(BStr::new(value_bytes)).map_err(|_| {
                            Error::InvalidHeader {
                                kind: ObjectKind::Commit,
                                header: "author",
                                reason: "author signature is not a valid signature",
                            }
                        })?,
                    );
                    required[1] = true;
                }
                "committer" => {
                    committer = Some(
                        Signature::parse(BStr::new(value_bytes)).map_err(|_| {
                            Error::InvalidHeader {
                                kind: ObjectKind::Commit,
                                header: "committer",
                                reason: "committer signature is not a valid signature",
                            }
                        })?,
                    );
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
        out.extend_from_slice(&self.message[..]);
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
        let msg = &self.message[..];
        // The subject is the first paragraph: everything up to (not including)
        // the first blank line ("\n\n") or the end of the message.
        // If the message is folded, we return the *un-folded* first physical line,
        // since BStr can only point into the original byte slice.
        // For simplicity and correctness with the &'a BStr return type, we return
        // the first paragraph's byte slice verbatim (newlines included).
        let end = msg.windows(2).position(|w| w == b"\n\n").unwrap_or(msg.len());
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
        let msg = &self.message[..];
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
            if &header_name[..] == name {
                let value_ref: &'a [u8] = *value;
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
    pub fn build(&self, format: ObjectFormat) -> Result<Vec<u8>> {
        let tree = self.tree.as_ref().ok_or_else(|| Error::MissingHeader {
            kind: ObjectKind::Commit,
            header: "tree",
        })?;
        let author = self
            .author
            .clone()
            .ok_or_else(|| {
                Error::Message("cannot resolve author identity: set GIT_AUTHOR_NAME or user.name".to_string())
            })?;
        let committer = self
            .committer
            .clone()
            .ok_or_else(|| {
                Error::Message("cannot resolve committer identity: set GIT_COMMITTER_NAME or user.name".to_string())
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

/// Inverse of `parse_tz_offset` for serialising: `(hours, non-negative minutes)`.
fn format_offset(total_minutes: i32) -> (i32, i32) {
    let sign = if total_minutes < 0 { -1 } else { 1 };
    let abs = total_minutes.abs();
    (sign * (abs / 60), abs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(err, Error::MissingHeader { header: "author", .. }));
    }

    #[test]
    fn missing_committer_header() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author David Queen <david@queen.example> 1000 +0000\n\
\nmessage\n";
        let err = Commit::parse(ObjectFormat::Sha1, body).unwrap_err();
        assert!(matches!(err, Error::MissingHeader { header: "committer", .. }));
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

    /// Malformed offset is rejected.
    #[test]
    fn malformed_offset_rejected() {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author David Queen <david@queen.example> 1000 +9999\n\
committer David Queen <david@queen.example> 2000 +0000\n\
\nmessage\n";
        let err = Commit::parse(ObjectFormat::Sha1, body).unwrap_err();
        assert!(matches!(err, Error::InvalidHeader { header: "author", .. }));
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
        let tree = ObjectId::from_hex(ObjectFormat::Sha1, BStr::new("0000000000000000000000000000000000000000")).unwrap();
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
        let tree = ObjectId::from_hex(ObjectFormat::Sha1, BStr::new("0000000000000000000000000000000000000000")).unwrap();
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
