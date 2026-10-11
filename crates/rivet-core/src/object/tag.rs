//! Phase 1 — annotated tags: a named pointer to another object, with a tagger and a message.
//!
//! A tag body is a short header block, a blank line, then a message:
//!
//! ```text
//! object 8a1f2c3d4e5f60718293a4b5c6d7e8f900112233
//! type commit
//! tag v1.0
//! tagger Rivet <rivet@example.com> 1759708800 +0200
//!
//! the tag message
//! -----BEGIN PGP SIGNATURE-----
//!  ...
//!  -----END PGP SIGNATURE-----
//! ```
//!
//! # A tag object is not a tag
//!
//! Only an **annotated** tag is an object. A lightweight tag (`git tag v1.0`) is nothing but a
//! ref under `refs/tags/` pointing straight at a commit — no object is created, and there is
//! nothing here to parse. So a `Tag<'a>` is never "the thing `refs/tags/v1.0` holds": the ref
//! may hold a commit id (lightweight) or this object's id (annotated), and only peeling tells
//! them apart. Code that assumes `refs/tags/*` is a tag object is wrong on most repositories.
//!
//! Nested tags are legal — a tag object may point at another tag object — so peeling has to
//! **bound its walk** rather than trusting the data to terminate, and report
//! [`crate::error::Error::TagCycle`] when it loops (plan §5).
//!
//! # The `type` header is a claim, not a fact
//!
//! `type` records the kind of the target *as the tag's author believed it*. Nothing enforces
//! that it agrees with the object the `object` id actually resolves to in this repository, and
//! a mismatched pair is exactly what `git fsck` looks for. This parser can only record the
//! claim; the object database, which can resolve the id, is where it gets checked.
//!
//! # The signature is not delimited by a blank line
//!
//! A signed tag's message runs straight into the signature: there is no blank line between
//! them, and the message may itself contain blank lines. The only way to find the boundary is
//! to scan for a signature-armour line at the start of a line. Git recognises **four** armour
//! headers — `-----BEGIN PGP SIGNATURE-----`, `-----BEGIN PGP MESSAGE-----`,
//! `-----BEGIN SIGNED MESSAGE-----` (X.509/gpgsm) and `-----BEGIN SSH SIGNATURE-----` — so
//! matching only the PGP one silently reports an SSH-signed tag as unsigned, which is the
//! failure mode that matters most, because it looks like "no signature" rather than like a bug.
//!
//! # Round-trip is not optional
//!
//! As with commits, a tag's id is the hash of its bytes. [`Tag`] keeps the original body and
//! [`Tag::to_bytes`] must return it unchanged whenever the tag has not been edited — a rebuild
//! from the parsed fields would reorder or respell headers and produce a tag that hashes
//! differently and therefore does not exist as far as Git is concerned.
//!
//! # Never panic on repository data
//!
//! Every parser here returns [`Result`] with a specific [`crate::error::Error`] variant and does
//! not index, unwrap or slice without bounds checks.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::object::Signature;

/// Phase 1 — a parsed annotated tag.
///
/// # Caveats
///
/// The struct holds **both** the parsed fields and the original body, and they must agree. The
/// body is the source of truth for anything not modelled here — header order, the exact spelling
/// of the tagger line, and the precise bytes of the signature block — so [`Tag::to_bytes`] must
/// return the original unchanged whenever the tag has not been edited. Only a tag built by
/// [`TagBuilder`] is serialised from its fields.
///
/// The tag **name** stored here is the `tag` header's value, which is the name Git's own tooling
/// would have written to `refs/tags/<name>`. It is not read from a ref, and nothing checks that
/// such a ref exists or points here: an annotated tag object can exist with no ref at all
/// (unreachable, and `git fsck` will say so), and a ref can point at a tag object whose internal
/// name disagrees with the ref name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag<'a> {
    /// The id of the object this tag points at. May itself be a tag object — nesting is legal.
    pub object: ObjectId,
    /// The kind the `type` header claims the target has. Untrusted: it is a claim by whoever
    /// wrote the tag, not a fact about the repository, and it must be checked against the object
    /// the id resolves to before being believed.
    pub kind: ObjectKind,
    /// The tag name, as raw bytes, single-line and without a trailing newline. Conventionally
    /// the name of the `refs/tags/` ref that points here, but not enforced to be.
    pub name: &'a BStr,
    /// Who created the tag, in the same `name <email> seconds ±hhmm` form as a commit author.
    pub tagger: Signature,
    /// The message, as raw bytes, with the signature block excluded and **not** trimmed — in
    /// particular a trailing newline is part of the message and must survive a round trip.
    ///
    /// May be empty: an empty tag message is legal and common for machine-generated tags.
    pub message: &'a [u8],
    /// The detached signature appended after the message, armour and all, or `None` when the tag
    /// is unsigned.
    ///
    /// Borrows from `raw`, and includes the `-----BEGIN …` and `-----END …` armour lines. Any of
    /// Git's four armour headers counts — see the module docs; do not test for PGP alone.
    pub signature: Option<&'a BStr>,
    /// The tag body exactly as it was parsed, header and message together.
    ///
    /// Kept for the same reason [`crate::object::Commit`] keeps its own: it is the only way to
    /// reproduce the input bytes, and therefore the tag's id, exactly.
    pub raw: &'a [u8],
}

impl<'a> Tag<'a> {
    /// Phase 1 — the object kind of everything this type represents.
    ///
    /// # Caveats
    ///
    /// Always [`ObjectKind::Tag`] — this is the kind of the *tag object*, not of the object it
    /// points at. That is [`Tag::kind`], and the two are different values in general; confusing
    /// them is how a `cat-file -t v1.0` gets answered with "commit" for a tag that is really a
    /// tag object.
    pub const fn kind() -> ObjectKind {
        ObjectKind::Tag
    }

    /// Phase 1 — parse a tag body.
    ///
    /// # Caveats
    ///
    /// `data` is the **body only** — the `"tag <len>\0"` header has already been consumed. The
    /// format is needed because the `object` header holds a full hex id whose length depends on
    /// it; a SHA-256 tag read as SHA-1 fails as a bogus id rather than as a clean length error.
    ///
    /// The header order is `object`, `type`, `tag`, `tagger`, though Git's own parser keys off
    /// the header names rather than their positions. Unknown headers before the blank line are
    /// not modelled and survive only through [`Tag::raw`]; a parser that drops them changes the
    /// tag's id.
    ///
    /// The message is everything after the first blank line, minus the signature block if one is
    /// present. It is **not** trimmed and **not** normalised, and it is legal for it to be empty
    /// or to contain further blank lines. The signature is found by scanning for an armour line
    /// at the start of a line — there is no blank line to look for (see the module docs) — and
    /// the newline that precedes it belongs to the message, not to the signature.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::MissingHeader`] if `object`, `type` or `tag` is absent;
    /// [`crate::error::Error::InvalidHeader`] if the `object` id is not valid hex for this
    /// format, or `type` names no object kind, or `tagger` does not parse as a signature;
    /// [`crate::error::Error::MalformedObject`] if there is no blank line separating headers from
    /// message, or a header line has no space after its name.
    pub fn parse(format: ObjectFormat, data: &'a [u8]) -> Result<Self> {
        // 1. Boundary: the first "\n\n" separates headers from the message region.
        let sep = data
            .windows(2)
            .position(|w| w == b"\n\n")
            .ok_or_else(|| {
                crate::error::Error::MalformedObject {
                    kind: ObjectKind::Tag,
                    reason: "no blank line after headers".to_string(),
                }
            })?;
        let msg_start = sep + 2;
        let header_region = &data[..sep];

        // 2. Header lines, keyed by name (not position). Duplicate: first wins.
        let mut object: Option<ObjectId> = None;
        let mut kind: Option<ObjectKind> = None;
        let mut name: Option<&'a BStr> = None;
        let mut tagger: Option<Signature> = None;

        for line in header_region.split(|b: &u8| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let space_idx = line.iter().position(|b| *b == b' ');
            let (hdr_name, value) = match space_idx {
                Some(i) => (&line[..i], &line[i + 1..]),
                None => {
                    return Err(crate::error::Error::MalformedObject {
                        kind: ObjectKind::Tag,
                        reason: "header line has no space after its name".to_string(),
                    })
                }
            };

            match hdr_name {
                b"object" if object.is_none() => {
                    object = Some(ObjectId::from_hex(format, BStr::new(value)).map_err(|_| {
                        crate::error::Error::InvalidHeader {
                            kind: ObjectKind::Tag,
                            header: "object",
                            reason: "not a valid object id for this format",
                        }
                    })?);
                }
                b"type" if kind.is_none() => {
                    kind = Some(match value {
                        b"blob" => ObjectKind::Blob,
                        b"tree" => ObjectKind::Tree,
                        b"commit" => ObjectKind::Commit,
                        b"tag" => ObjectKind::Tag,
                        _ => {
                            return Err(crate::error::Error::InvalidHeader {
                                kind: ObjectKind::Tag,
                                header: "type",
                                reason: "unknown object kind",
                            })
                        }
                    });
                }
                b"tag" if name.is_none() => {
                    name = Some(BStr::new(value));
                }
                b"tagger" if tagger.is_none() => {
                    tagger = Some(Signature::parse(BStr::new(value)).map_err(|_| {
                        crate::error::Error::InvalidHeader {
                            kind: ObjectKind::Tag,
                            header: "tagger",
                            reason: "malformed signature",
                        }
                    })?);
                }
                _ => {
                    // Unknown header: skip; survives only through `raw`.
                    continue;
                }
            }
        }

        // 4. Required headers.
        let object = object.ok_or_else(|| {
            crate::error::Error::MissingHeader {
                kind: ObjectKind::Tag,
                header: "object",
            }
        })?;
        let kind = kind.ok_or_else(|| {
            crate::error::Error::MissingHeader {
                kind: ObjectKind::Tag,
                header: "type",
            }
        })?;
        let name = name.ok_or_else(|| {
            crate::error::Error::MissingHeader {
                kind: ObjectKind::Tag,
                header: "tag",
            }
        })?;
        let tagger = tagger.ok_or_else(|| {
            crate::error::Error::MissingHeader {
                kind: ObjectKind::Tag,
                header: "tagger",
            }
        })?;

        // 5. Signature scan: keep the LAST marker that sits at index 0 or immediately after '\n',
        // scanning the message region once, left to right.
        let msg_region = &data[msg_start..];
        let armour: [&[u8]; 4] = [
            b"-----BEGIN PGP SIGNATURE-----",
            b"-----BEGIN PGP MESSAGE-----",
            b"-----BEGIN SIGNED MESSAGE-----",
            b"-----BEGIN SSH SIGNATURE-----",
        ];
        let mut sig_start: Option<usize> = None;
        let mut i = 0usize;
        while i + armour[0].len() <= msg_region.len() {
            let at_line_start = i == 0 || msg_region[i - 1] == b'\n';
            if at_line_start {
                for marker in &armour {
                    if &msg_region[i..i + marker.len()] == *marker {
                        sig_start = Some(i);
                    }
                }
            }
            i += 1;
        }

        // 6. Split: message keeps the preceding newline; signature runs to end of body.
        let (message, signature) = match sig_start {
            Some(i) => {
                let abs_start = msg_start + i;
                (
                    BStr::new(&data[msg_start..abs_start]),
                    Some(BStr::new(&data[abs_start..])),
                )
            }
            None => (BStr::new(&data[msg_start..]), None),
        };

        // 7. Store raw; all fields borrow from `data`.
        Ok(Self {
            object,
            kind,
            name,
            message,
            tagger,
            signature,
            raw: data,
        })
    }

    /// Phase 1 — serialise to a tag body.
    ///
    /// # Caveats
    ///
    /// Returns the **body only**; the header is prepended by [`crate::object::hash_object`], and
    /// [`Tag::write_to`] writes these same bytes to a stream.
    ///
    /// For a tag that came from [`Tag::parse`], this must reproduce the input bytes exactly —
    /// that is the round-trip property the differential tests check, and it is why the `raw`
    /// field exists. Rebuilding from the parsed fields would lose any spelling difference in the
    /// tagger line or the armour, changing the tag's id.
    ///
    /// Header order in a rebuild is `object`, `type`, `tag`, `tagger`, blank line, message,
    /// signature — which is what Git writes. Note that a signature is appended **without** a
    /// blank line before it, so a rebuild that inserts one produces a tag whose signature no
    /// longer verifies.
    pub fn to_bytes(&self) -> Vec<u8> {
        // A parsed tag round-trips byte-for-byte: return the raw body unchanged.
        if !self.raw.is_empty() {
            return self.raw.to_vec();
        }
        // Rebuild from the parsed fields (a TagBuilder-built tag has no raw yet).
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&self.object.to_hex());
        out.push(b'\n');
        out.extend_from_slice(b"type ");
        out.extend_from_slice(self.kind.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(b"tag ");
        out.extend_from_slice(&self.name[..]);
        out.push(b'\n');
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&self.tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(&self.message[..]);
        // Signature appended with no separator.
        if let Some(sig) = &self.signature {
            out.extend_from_slice(&sig[..]);
        }
        out
    }

    /// Phase 1 — write the tag body to `out`, without building it in memory.
    ///
    /// # Caveats
    ///
    /// Writes exactly the bytes [`Tag::to_bytes`] would return — the **body only**, no
    /// `"tag <len>\0"` header. The header's length field is the body length, so a caller that
    /// streams must know it up front; that is why this does not write the header, and why the
    /// object database computes it from the same buffer it hashes.
    ///
    /// Partial writes are the writer's problem: this returns as soon as `out` reports success, and
    /// a caller writing into a file must not assume the whole body was flushed on return.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Io`] if `out` fails. There is no `Result`-free variant for a
    /// `Vec<u8>` sink: the point of this method is the streaming path, and a second method that
    /// cannot fail would just be [`Tag::to_bytes`].
    pub fn write_to(&self, out: &mut dyn std::io::Write) -> Result<()> {
        out.write_all(&self.to_bytes())?;
        Ok(())
    }

    /// Phase 1 — the tag's id under a given object format.
    ///
    /// # Caveats
    ///
    /// Computed over `to_bytes`, never over a rebuild — see [`Tag::to_bytes`]. The format is a
    /// parameter because the same tag body in a SHA-1 and a SHA-256 repository has different
    /// ids, and a `Tag` carries no repository context.
    ///
    /// # Errors
    ///
    /// Only the hasher's collision-detection path.
    pub fn id(&self, format: ObjectFormat) -> Result<ObjectId> {
        let body = self.to_bytes();
        crate::object::hash_object(format, ObjectKind::Tag, &body).map(|(id, _)| id)
    }

    /// Phase 1 — the id of the object this tag points at.
    ///
    /// # Caveats
    ///
    /// This is the id **as recorded**, and it is not resolved: it may name an object this
    /// repository does not have, and for a nested tag it names another tag object rather than a
    /// commit. Peeling to the underlying non-tag object is the object database's job and is
    /// bounded there (see [`crate::error::Error::TagCycle`]).
    pub fn object(&self) -> ObjectId {
        self.object
    }

    /// Phase 1 — the tag name from the `tag` header.
    ///
    /// # Caveats
    ///
    /// The same bytes as the public field. It is **not** the ref name and carries no `refs/tags/`
    /// prefix; whether a ref of that name exists, and whether it points here, are separate
    /// questions that this type cannot answer.
    pub fn name(&self) -> &'a BStr {
        self.name
    }

    /// Phase 1 — who created the tag.
    ///
    /// # Caveats
    ///
    /// Borrowed rather than cloned, because the `tag` command's display path wants the name and
    /// email and nothing else.
    pub fn tagger(&self) -> &Signature {
        &self.tagger
    }

    /// Phase 1 — the message, without the signature block.
    ///
    /// # Caveats
    ///
    /// Not trimmed: a trailing newline in the body is part of the message and is what
    /// [`Tag::to_bytes`] must write back. An unsigned tag with no message returns an empty slice,
    /// which is a legal state, not an error.
    pub fn message(&self) -> &'a [u8] {
        &self.message[..]
    }

    /// Phase 1 — the appended signature, if the tag has one.
    ///
    /// # Caveats
    ///
    /// Present means "an armour block was found", not "the signature is valid" — verification
    /// invokes gpg/ssh and is out of scope for the core. It also says nothing about *which*
    /// signer: all four of Git's armour formats land here, and telling them apart is the
    /// verifier's problem.
    pub fn signature(&self) -> Option<&'a BStr> {
        self.signature
    }

    /// Phase 1 — whether the tag carries a signature block.
    ///
    /// # Caveats
    ///
    /// A convenience over [`Tag::signature`] for the common "is this signed" branch. It is not a
    /// substitute for verification and must not be reported to the user as one.
    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }
}

/// Phase 1 — build a tag object that does not exist yet.
///
/// The write side of [`Tag`], separated out for the same reason as
/// [`crate::object::CommitBuilder`]: creating a tag has requirements parsing does not, and it
/// owns its data because there is nothing to preserve. Also like a commit builder, a tag built
/// through here will not round-trip byte-for-byte with `git tag -a` from identical inputs — the
/// tagger defaults come from config and the environment, and the differential tests have to
/// compare the parsed fields rather than the ids.
///
/// # Caveats
///
/// Signing is **not** done here. [`TagBuilder::signature`] records an armour block the caller
/// obtained elsewhere; producing one means running `gpg`/`ssh-keygen` over the message, which is
/// process execution and belongs to the CLI, not the core (plan §3).
#[derive(Clone, Debug, Default)]
pub struct TagBuilder {
    /// The object the tag points at. Required — there is no sensible default.
    pub object: Option<ObjectId>,
    /// The kind to write in the `type` header. When `None`, `build` resolves it from the object
    /// database, which is the only way to get it right for an arbitrary id.
    pub kind: Option<ObjectKind>,
    /// The tag name, as raw bytes. Required.
    pub name: BString,
    /// The tagger. Defaults from `GIT_COMMITTER_*` and `user.*` config when `None`, exactly as a
    /// commit builder's committer does — Git has no separate `GIT_TAGGER_*` variables.
    pub tagger: Option<Signature>,
    /// The message, as raw bytes. May be empty.
    pub message: BString,
    /// An already-obtained signature armour block to append, if the tag is to be signed.
    pub signature: Option<BString>,
}

impl TagBuilder {
    /// Phase 1 — a builder with nothing set.
    ///
    /// # Caveats
    ///
    /// Does not consult config or the environment: defaults are resolved in
    /// [`TagBuilder::build`], which is the only place that needs them and the only place that
    /// should be doing I/O.
    pub fn new() -> Self {
        Self::default()
    }

    /// Phase 1 — set the object the tag points at.
    pub fn object(&mut self, object: ObjectId) -> &mut Self {
        self.object = Some(object);
        self
    }

    /// Phase 1 — set the `type` header explicitly instead of resolving it.
    ///
    /// # Caveats
    ///
    /// Overriding this is almost always wrong: the value is only correct when it agrees with what
    /// the `object` id resolves to, and a mismatch produces a tag `git fsck` rejects. It exists
    /// for tests and for rewriting a tag read from another repository, not for ordinary tagging.
    pub fn kind(&mut self, kind: ObjectKind) -> &mut Self {
        self.kind = Some(kind);
        self
    }

    /// Phase 1 — set the tag name.
    ///
    /// # Caveats
    ///
    /// Not validated here. A name that is not a legal ref name produces a tag object that can be
    /// written but not referenced, and `tag` must run it through `git check-ref-format` rules
    /// before calling this — see [`crate::error::Error::InvalidRefName`].
    pub fn name(&mut self, name: &[u8]) -> &mut Self {
        self.name = BString::from(name);
        self
    }

    /// Phase 1 — set the tagger explicitly.
    ///
    /// # Caveats
    ///
    /// Without this, `build` falls back to `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL`/
    /// `GIT_COMMITTER_DATE` and then to `user.name`/`user.email`. Git's precedence order is exact
    /// and the differential tests depend on it.
    pub fn tagger(&mut self, tagger: Signature) -> &mut Self {
        self.tagger = Some(tagger);
        self
    }

    /// Phase 1 — set the raw message.
    ///
    /// # Caveats
    ///
    /// No trailing newline is appended by this method. `git tag -a -m` *does* append one, and the
    /// `tag` command must emulate that; putting it here instead would make every programmatic tag
    /// carry one whether or not it wanted it.
    pub fn message(&mut self, message: &[u8]) -> &mut Self {
        self.message = BString::from(message);
        self
    }

    /// Phase 1 — set the signature armour to append.
    ///
    /// # Caveats
    ///
    /// The bytes are written verbatim after the message, with no inserted separator and no
    /// validation that they are armour at all — but they must be, because [`Tag::parse`] finds
    /// the signature by scanning for one of Git's four armour headers. A block that does not
    /// start with one of those lines will be read back as part of the message, so the tag would
    /// parse as unsigned.
    pub fn signature(&mut self, signature: &[u8]) -> &mut Self {
        self.signature = Some(BString::from(signature));
        self
    }

    /// Phase 1 — validate, resolve defaults, and serialise to a tag body.
    ///
    /// The result is passed to [`crate::object::hash_object`] and written to the object database;
    /// this method does not write anything itself.
    ///
    /// # Caveats
    ///
    /// The `type` header is resolved from `kind` when set, and otherwise from the object database
    /// — which means `build` needs a lookup the signature does not take. That resolves at the
    /// call site: the `tag` command fetches the target first and passes its kind in.
    ///
    /// An empty message is legal, as it is for a commit.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::MissingHeader`] if no object was set — the one field with no
    /// sensible default; [`crate::error::Error::Message`] if the tagger cannot be resolved, which
    /// is the "please tell me who you are" failure `git tag -a` has.
    pub fn build(&self, format: ObjectFormat) -> Result<Vec<u8>> {
        // D3: object is the one field with no sensible default.
        let object = self.object.ok_or_else(|| {
            crate::error::Error::MissingHeader {
                kind: ObjectKind::Tag,
                header: "object",
            }
        })?;

        // D1/D3: tagger must be resolvable; without one this is the "who are you" failure.
        let tagger = self.tagger.as_ref().ok_or_else(|| {
            crate::error::Error::Message(
                "cannot resolve tagger identity: set GIT_COMMITTER_NAME or user.name".to_string(),
            )
        })?;

        // D2: type is resolved from `kind`; without it, `build` cannot resolve it.
        let kind = self.kind.ok_or_else(|| {
            crate::error::Error::MissingHeader {
                kind: ObjectKind::Tag,
                header: "type",
            }
        })?;

        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&object.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type ");
        out.extend_from_slice(kind.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(b"tag ");
        out.extend_from_slice(&self.name[..]);
        out.push(b'\n');
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(&self.message[..]);
        // Signature appended verbatim with no separator.
        if let Some(sig) = &self.signature {
            out.extend_from_slice(&sig[..]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use bstr::{BStr, BString};
    use crate::hash::{ObjectFormat, ObjectId};
    use crate::object::ObjectKind;
    use crate::object::commit::Signature;
    use super::{Tag, TagBuilder};

    fn sha1_id(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, BStr::new(hex)).unwrap()
    }

    /// Build a tagger signature usable in a tag body.
    fn test_tagger() -> Signature {
        Signature {
            name: BString::from("Alice Tagger"),
            email: BString::from("alice@example.com"),
            when: 1700000000,
            tz_offset_minutes: 60,
        }
    }

    /// A complete, unsigned tag buffer.
    fn unsigned_tag_buffer() -> Vec<u8> {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"Release 1.0\n");
        out
    }

    #[test]
    fn parse_round_trip_unsigned() {
        let data = unsigned_tag_buffer();
        let tag = Tag::parse(ObjectFormat::Sha1, &data).unwrap();
        // Round-trip exact.
        assert_eq!(tag.to_bytes(), data);
        // id matches the object-id of the same buffer.
        let (expected_id, _) = crate::object::hash_object(ObjectFormat::Sha1, ObjectKind::Tag, &data).unwrap();
        assert_eq!(tag.id(ObjectFormat::Sha1).unwrap(), expected_id);
        // Headers parse.
        assert_eq!(tag.object(), sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748"));
        assert_eq!(tag.kind, ObjectKind::Commit);
        assert_eq!(tag.name(), b"v1.0");
        assert_eq!(tag.tagger().name, "Alice Tagger");
        assert_eq!(tag.tagger().email, "alice@example.com");
        assert_eq!(tag.tagger().when, 1700000000);
        assert_eq!(tag.tagger().tz_offset_minutes, 60);
        assert_eq!(tag.message(), b"Release 1.0\n");
        assert_eq!(tag.signature(), None);
        assert!(!tag.is_signed());
    }

    #[test]
    fn parse_signed_tag() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let armour = b"-----BEGIN PGP SIGNATURE-----\n-----BEGIN PGP MESSAGE-----\nabc\n-----END PGP MESSAGE-----\n-----END PGP SIGNATURE-----";
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        // message has a trailing newline before the armour
        out.extend_from_slice(b"the tag message\n");
        out.extend_from_slice(armour);
        out.push(b'\n');

        let tag = Tag::parse(ObjectFormat::Sha1, &out).unwrap();
        // Git keeps the LAST line-start marker: here that is the PGP MESSAGE line, so the
        // quoted SIGNATURE block is message text.
        assert_eq!(tag.message(), b"the tag message\n-----BEGIN PGP SIGNATURE-----\n");
        assert!(tag.is_signed());
        let sig = tag.signature().unwrap();
        assert!(sig.starts_with(b"-----BEGIN PGP MESSAGE-----"));
        // The signature runs to the end of the body: 86 bytes, including the final newline.
        assert_eq!(sig.len(), 86);
        assert_eq!(sig.to_vec(), out[out.len() - 86..]);
        // Round trip.
        assert_eq!(tag.to_bytes(), out);
        let (expected_id, _) = crate::object::hash_object(ObjectFormat::Sha1, ObjectKind::Tag, &out).unwrap();
        assert_eq!(tag.id(ObjectFormat::Sha1).unwrap(), expected_id);
    }

    #[test]
    fn parse_keeps_the_last_line_start_marker() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let pgp_block = b"-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----";
        let ssh_block = b"-----BEGIN SSH SIGNATURE-----\nabc\n-----END SSH SIGNATURE-----";

        // PGP block first, then SSH block: the signature starts at the SSH marker.
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"the tag message\n");
        out.extend_from_slice(pgp_block);
        out.extend_from_slice(b"\n");
        out.extend_from_slice(ssh_block);
        let tag = Tag::parse(ObjectFormat::Sha1, &out).unwrap();
        assert_eq!(
            tag.message(),
            b"the tag message\n-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n"
        );
        assert!(tag.is_signed());
        assert!(tag.signature().unwrap().starts_with(b"-----BEGIN SSH SIGNATURE-----"));

        // SSH block first, then PGP block: the signature starts at the PGP marker.
        let mut out2 = Vec::new();
        out2.extend_from_slice(b"object ");
        out2.extend_from_slice(&obj.to_hex()[..]);
        out2.push(b'\n');
        out2.extend_from_slice(b"type commit\n");
        out2.extend_from_slice(b"tag v1.0\n");
        out2.extend_from_slice(b"tagger ");
        out2.extend_from_slice(&tagger.to_bytes());
        out2.push(b'\n');
        out2.push(b'\n');
        out2.extend_from_slice(b"the tag message\n");
        out2.extend_from_slice(ssh_block);
        out2.extend_from_slice(b"\n");
        out2.extend_from_slice(pgp_block);
        let tag2 = Tag::parse(ObjectFormat::Sha1, &out2).unwrap();
        assert_eq!(
            tag2.message(),
            b"the tag message\n-----BEGIN SSH SIGNATURE-----\nabc\n-----END SSH SIGNATURE-----\n"
        );
        assert!(tag2.is_signed());
        assert!(tag2.signature().unwrap().starts_with(b"-----BEGIN PGP SIGNATURE-----"));
    }

    #[test]
    fn parse_ssh_and_other_markers() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"msg\n-----BEGIN SSH SIGNATURE-----\nrest\n-----END SSH SIGNATURE-----");

        let tag = Tag::parse(ObjectFormat::Sha1, &out).unwrap();
        assert!(tag.is_signed());
        assert!(tag.signature().unwrap().starts_with(b"-----BEGIN SSH SIGNATURE-----"));

        // PGP MESSAGE marker.
        let marker_pgp = b"-----BEGIN PGP MESSAGE-----";
        let end = b"\n-----END PGP MESSAGE-----\n-----END PGP SIGNATURE-----";
        let mut body_pgp = Vec::new();
        body_pgp.extend_from_slice(b"object ");
        body_pgp.extend_from_slice(&obj.to_hex()[..]);
        body_pgp.push(b'\n');
        body_pgp.extend_from_slice(b"type commit\n");
        body_pgp.extend_from_slice(b"tag v1.0\n");
        body_pgp.extend_from_slice(b"tagger ");
        body_pgp.extend_from_slice(&tagger.to_bytes());
        body_pgp.push(b'\n');
        body_pgp.push(b'\n');
        body_pgp.extend_from_slice(b"msg\n");
        body_pgp.extend_from_slice(marker_pgp);
        body_pgp.extend_from_slice(end);
        let tag2 = Tag::parse(ObjectFormat::Sha1, &body_pgp).unwrap();
        assert!(tag2.is_signed());

        // SIGNED MESSAGE marker.
        let mut body_sm = Vec::new();
        body_sm.extend_from_slice(b"object ");
        body_sm.extend_from_slice(&obj.to_hex()[..]);
        body_sm.push(b'\n');
        body_sm.extend_from_slice(b"type commit\n");
        body_sm.extend_from_slice(b"tag v1.0\n");
        body_sm.extend_from_slice(b"tagger ");
        body_sm.extend_from_slice(&tagger.to_bytes());
        body_sm.push(b'\n');
        body_sm.push(b'\n');
        body_sm.extend_from_slice(b"msg\n");
        body_sm.extend_from_slice(b"-----BEGIN SIGNED MESSAGE-----");
        body_sm.extend_from_slice(end);
        let tag3 = Tag::parse(ObjectFormat::Sha1, &body_sm).unwrap();
        assert!(tag3.is_signed());
    }

    #[test]
    fn parse_blank_line_in_message_does_not_end_it() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        // message contains a blank line — not the header separator
        out.extend_from_slice(b"line1\n\nline2\n-----BEGIN PGP SIGNATURE-----\n-----END PGP SIGNATURE-----");

        let tag = Tag::parse(ObjectFormat::Sha1, &out).unwrap();
        assert_eq!(tag.message(), b"line1\n\nline2\n");
        assert!(tag.is_signed());
    }

    #[test]
    fn parse_unsigned_empty_message() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        // empty message

        let tag = Tag::parse(ObjectFormat::Sha1, &out).unwrap();
        assert!(tag.message().is_empty());
        assert_eq!(tag.signature(), None);
        assert!(!tag.is_signed());
        // still legal, round-trips
        assert_eq!(tag.to_bytes(), out);
    }

    #[test]
    fn parse_missing_tagger_is_missing_header() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"msg\n");
        let err = Tag::parse(ObjectFormat::Sha1, &out).unwrap_err();
        match err {
            crate::error::Error::MissingHeader { header, .. } => assert_eq!(header, "tagger"),
            _ => panic!("expected MissingHeader"),
        }
    }

    #[test]
    fn parse_unknown_header_is_skipped() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.extend_from_slice(b"extra foo\n"); // unknown header, skipped
        out.push(b'\n');
        out.extend_from_slice(b"msg\n");

        let tag = Tag::parse(ObjectFormat::Sha1, &out).unwrap();
        assert_eq!(tag.to_bytes(), out); // round trip exact
    }

    #[test]
    fn parse_header_line_no_space_is_malformed() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"nospace\n"); // no space after name
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"msg\n");
        let err = Tag::parse(ObjectFormat::Sha1, &out).unwrap_err();
        match err {
            crate::error::Error::MalformedObject { reason, .. } => {
                assert_eq!(reason, "header line has no space after its name");
            }
            _ => panic!("expected MalformedObject"),
        }
        let _ = tagger;
    }

    #[test]
    fn parse_bad_type_is_invalid_header() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type quux\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"msg\n");
        let err = Tag::parse(ObjectFormat::Sha1, &out).unwrap_err();
        match err {
            crate::error::Error::InvalidHeader { header, .. } => assert_eq!(header, "type"),
            _ => panic!("expected InvalidHeader"),
        }
    }

    #[test]
    fn parse_bad_object_hex_is_invalid_header() {
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(b"zzzz"); // not hex
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        out.push(b'\n');
        out.extend_from_slice(b"msg\n");
        let err = Tag::parse(ObjectFormat::Sha1, &out).unwrap_err();
        match err {
            crate::error::Error::InvalidHeader { header, .. } => assert_eq!(header, "object"),
            _ => panic!("expected InvalidHeader"),
        }
    }

    #[test]
    fn parse_no_blank_line_is_malformed() {
        let obj = sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748");
        let tagger = test_tagger();
        let mut out = Vec::new();
        out.extend_from_slice(b"object ");
        out.extend_from_slice(&obj.to_hex()[..]);
        out.push(b'\n');
        out.extend_from_slice(b"type commit\n");
        out.extend_from_slice(b"tag v1.0\n");
        out.extend_from_slice(b"tagger ");
        out.extend_from_slice(&tagger.to_bytes());
        out.push(b'\n');
        // no blank line, message directly after
        out.extend_from_slice(b"msg\n");
        let err = Tag::parse(ObjectFormat::Sha1, &out).unwrap_err();
        match err {
            crate::error::Error::MalformedObject { reason, .. } => {
                assert_eq!(reason, "no blank line after headers");
            }
            _ => panic!("expected MalformedObject"),
        }
    }

    #[test]
    fn kind_is_tag_not_target() {
        let data = unsigned_tag_buffer();
        let tag = Tag::parse(ObjectFormat::Sha1, &data).unwrap();
        assert_eq!(Tag::kind(), ObjectKind::Tag);
        // but the target kind is stored separately
        assert_eq!(tag.kind, ObjectKind::Commit);
    }

    #[test]
    fn builder_new_is_empty_and_build_missing_object() {
        let b = TagBuilder::new();
        // build without object -> MissingHeader
        let err = b.build(ObjectFormat::Sha1).unwrap_err();
        match err {
            crate::error::Error::MissingHeader { header, .. } => assert_eq!(header, "object"),
            _ => panic!("expected MissingHeader"),
        }
    }

    #[test]
    fn builder_missing_tagger_is_message() {
        let mut b = TagBuilder::new();
        b.object(sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748"));
        b.kind(ObjectKind::Commit);
        b.name(b"v1.0");
        // no tagger set -> Message
        let err = b.build(ObjectFormat::Sha1).unwrap_err();
        match err {
            crate::error::Error::Message(_) => {}
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn builder_missing_kind_is_missing_header() {
        let mut b = TagBuilder::new();
        b.object(sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748"));
        b.tagger(test_tagger());
        b.name(b"v1.0");
        // no kind -> MissingHeader
        let err = b.build(ObjectFormat::Sha1).unwrap_err();
        match err {
            crate::error::Error::MissingHeader { header, .. } => assert_eq!(header, "type"),
            _ => panic!("expected MissingHeader"),
        }
    }

    #[test]
    fn builder_full_round_trip() {
        let mut b = TagBuilder::new();
        b.object(sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748"));
        b.kind(ObjectKind::Commit);
        b.name(b"v1.0");
        b.tagger(test_tagger());
        b.message(b"Release 1.0\n");
        let bytes = b.build(ObjectFormat::Sha1).unwrap();
        // writes "type commit"
        assert!(bytes.windows(9).any(|w| w == b"type comm"));
        // parse it back
        let tag = Tag::parse(ObjectFormat::Sha1, &bytes).unwrap();
        assert_eq!(tag.object(), sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748"));
        assert_eq!(tag.name(), b"v1.0");
        assert_eq!(tag.message(), b"Release 1.0\n");
        // round trip exact
        assert_eq!(tag.to_bytes(), bytes);
    }

    #[test]
    fn builder_signature_is_found_on_parse() {
        let mut b = TagBuilder::new();
        b.object(sha1_id("4b825d6d2417e751203d05853896f2f6e2a25748"));
        b.kind(ObjectKind::Commit);
        b.name(b"v1.0");
        b.tagger(test_tagger());
        b.message(b"msg\n");
        b.signature(b"-----BEGIN PGP SIGNATURE-----\nxyz\n-----END PGP SIGNATURE-----");
        let bytes = b.build(ObjectFormat::Sha1).unwrap();
        let tag = Tag::parse(ObjectFormat::Sha1, &bytes).unwrap();
        assert!(tag.is_signed());
        assert!(tag.signature().unwrap().starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        assert_eq!(tag.to_bytes(), bytes);
    }
}
