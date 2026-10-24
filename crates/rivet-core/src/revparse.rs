//! Revision specifications, resolved the way `git rev-parse` resolves them.
//!
//! A revision spec is anything a user types where Git wants an object: `HEAD`, `main`,
//! `refs/tags/v1.0`, a full or abbreviated object id, `HEAD~3`, `main^2`, `v1.0^{}`, and so on.
//! [`revwalk`](crate::revwalk) parses the *shape* of a spec without touching a repository; this
//! module is the half that needs one — it looks names up in the ref store, expands abbreviations
//! against the object database, and applies the suffix operators.
//!
//! # Caveats
//!
//! * **Resolution is lazy about kinds.** [`resolve`] returns whatever object the spec names, not
//!   necessarily a commit: `rev-parse v1.0` on an annotated tag yields the *tag* object, matching
//!   Git, and it is the caller that decides whether to peel. Use [`to_tree`] or
//!   [`peel_to_commit`] when the caller cannot work with anything else.
//! * **`^N` counts from one**, as in Git, so `HEAD^` and `HEAD^1` are the first parent. `^0`
//!   means "this commit, peeled from any tag" and is not a parent reference.
//! * **A date-like spec (`main@{yesterday}`) is not supported** and is reported as an invalid
//!   revision rather than silently resolved to something else.
use bstr::{BStr, BString, ByteSlice};

use crate::error::{Error, Result};
use crate::hash::ObjectFormat;
use crate::hash::ObjectId;
use crate::object::{Commit, EntryMode, Object, Tag, Tree};
use crate::odb::Odb;
use crate::refs::{RefName, RefStore};

/// A suffix operator, applied left to right after the base has been resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `^N` — the *N*th parent. `^0` peels a tag to its commit.
    Parent(u32),
    /// `~N` — the *N*th first-parent ancestor.
    FirstParent(u32),
    /// `^{}` and `^{type}` — peel tags and check the kind.
    Peel(Option<PeelKind>),
}

/// The kind named inside `^{…}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeelKind {
    Commit,
    Tree,
    Blob,
    Tag,
    Object,
}

/// Resolves a revision spec to an object id.
///
/// # Errors
///
/// [`Error::InvalidRevision`] when the spec is not something Git would accept — including a name
/// that resolves to nothing, since distinguishing "cannot parse this" from "this names nothing"
/// matters to `rev-parse`'s exit status but not to its output.
///
/// [`Error::ObjectNotFound`] when the spec names an id the database does not hold.
///
/// [`Error::AmbiguousPrefix`] when an abbreviation matches more than one object.
pub fn resolve(format: ObjectFormat, refs: &RefStore, odb: &Odb, spec: &BStr) -> Result<ObjectId> {
    if spec.is_empty() {
        return Err(invalid(spec, "empty revision"));
    }

    // Peel suffixes off the right, remembering them, because `HEAD~2^{commit}` applies `~2`
    // before the peel even though the peel is written last.
    let mut ops = Vec::new();
    let mut base = spec;
    while let Some((rest, op)) = split_suffix(base)? {
        ops.push(op);
        base = rest;
        if base.is_empty() {
            return Err(invalid(spec, "suffix has no revision"));
        }
    }

    let mut oid = resolve_base(format, refs, odb, base)?;
    for op in ops.iter().rev() {
        oid = apply(format, odb, oid, *op, spec)?;
    }
    Ok(oid)
}

/// Resolves `REV:path` (or a bare `REV`) to the object at that path.
///
/// The empty path (`HEAD:`) resolves to the tree itself, as in Git.
pub fn resolve_path(
    format: ObjectFormat,
    refs: &RefStore,
    odb: &Odb,
    spec: &BStr,
) -> Result<ObjectId> {
    let (rev, path) = match spec.find_byte(b':') {
        Some(colon) => (&spec[..colon], &spec[colon + 1..]),
        None => return resolve(format, refs, odb, spec),
    };
    let mut oid = to_tree(format, refs, odb, rev)?;
    if path.is_empty() {
        return Ok(oid);
    }
    for part in path.split(|b| *b == b'/') {
        if part.is_empty() {
            return Err(invalid(spec, "empty path component"));
        }
        let object = odb.read(&oid)?;
        let tree = Tree::parse(format, object.data())?;
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.name == part)
            .ok_or_else(|| invalid(spec, "path does not exist in the tree"))?;
        if entry.mode == EntryMode::Commit {
            // A gitlink's "content" is a commit in another repository; there is nothing here to
            // descend into and Git reports the id rather than failing.
            return Err(invalid(spec, "path descends into a submodule"));
        }
        oid = entry.oid;
    }
    Ok(oid)
}

/// Peels a spec to a tree id: a commit becomes its tree, a tag is followed to one of the two.
pub fn to_tree(format: ObjectFormat, refs: &RefStore, odb: &Odb, spec: &BStr) -> Result<ObjectId> {
    let oid = resolve(format, refs, odb, spec)?;
    let object = peel_tags(format, odb, oid)?;
    match object.kind() {
        crate::object::ObjectKind::Tree => Ok(oid),
        crate::object::ObjectKind::Commit => Ok(Commit::parse(format, object.data())?.tree),
        crate::object::ObjectKind::Blob => Err(invalid(spec, "is a blob, not a tree")),
        crate::object::ObjectKind::Tag => unreachable!("peel_tags stops at a non-tag"),
    }
}

/// Peels a spec to a commit id, following annotated tags.
pub fn peel_to_commit(
    format: ObjectFormat,
    refs: &RefStore,
    odb: &Odb,
    spec: &BStr,
) -> Result<ObjectId> {
    let oid = resolve(format, refs, odb, spec)?;
    let object = peel_tags(format, odb, oid)?;
    match object.kind() {
        crate::object::ObjectKind::Commit => Ok(oid),
        _ => Err(invalid(spec, "is not a commit")),
    }
}

/// Resolves the base of a spec — everything before the first suffix operator.
fn resolve_base(format: ObjectFormat, refs: &RefStore, odb: &Odb, base: &BStr) -> Result<ObjectId> {
    if base == "HEAD" || base == "@" {
        // `head_commit` both follows a symbolic ref and peels an annotated tag on a detached
        // HEAD; it reports an unborn branch as an error rather than an id.
        return refs.head_commit(odb);
    }
    if let Some(oid) = resolve_reflog(refs, base)? {
        return Ok(oid);
    }
    if let Some(back) = parse_previous(base) {
        let Some(branch) = previous_checkout(refs, back) else {
            return Err(invalid(base, "does not name an object"));
        };
        return resolve_base(format, refs, odb, BStr::new(&branch));
    }
    if is_hex(base) {
        let hex_len = format.hex_len();
        if base.len() == hex_len {
            return ObjectId::from_hex(format, base);
        }
        if base.len() >= 4 && base.len() % 2 == 1 {
            // An `ObjectId` prefix holds whole bytes only, so the odd last nibble is matched
            // against the hex text of every id instead (`git log 2461a6d` is the common case).
            let wanted = base.to_ascii_lowercase();
            let mut found = Vec::new();
            for id in odb.oids()? {
                if id.to_hex().starts_with(&wanted) && !found.contains(&id) {
                    found.push(id);
                }
            }
            return match found.len() {
                1 => Ok(found[0]),
                0 => Err(Error::ObjectNotFound {
                    oid: ObjectId::from_hex_prefix(format, BStr::new(&base[..base.len() - 1]))?,
                }),
                n => Err(Error::AmbiguousPrefix {
                    prefix: base.to_owned(),
                    candidates: n,
                }),
            };
        }
        if base.len() >= 4 {
            let prefix = ObjectId::from_hex_prefix(format, base)?;
            return odb.resolve_prefix(&prefix);
        }
    }

    if let Some(name) = dwim_ref(refs, base) {
        return refs.resolve_oid(&name);
    }
    Err(invalid(base, "does not name an object"))
}

/// Finds the ref a short name abbreviates, in Git's order: the literal name first, then the
/// namespaces a short name usually abbreviates. A full ref name that exists must win over any
/// tagged name of the same text.
fn dwim_ref(refs: &RefStore, base: &BStr) -> Option<RefName> {
    let mut candidates = vec![base.to_owned()];
    for prefix in ["refs/", "refs/tags/", "refs/heads/", "refs/remotes/"] {
        let mut name = BString::from(prefix);
        name.extend_from_slice(base);
        candidates.push(name);
    }
    let mut remote_head = BString::from("refs/remotes/");
    remote_head.extend_from_slice(base);
    remote_head.extend_from_slice(b"/HEAD");
    candidates.push(remote_head);

    candidates.into_iter().find_map(|candidate| {
        let name = RefName::from_bytes(&candidate).ok()?;
        refs.read(&name).is_ok().then_some(name)
    })
}

/// The branch (or detached id) `n` checkouts ago, from `checkout: moving from X to Y` entries in
/// the `HEAD` reflog, newest first. `None` when `HEAD` has not moved that often.
fn previous_checkout(refs: &RefStore, n: usize) -> Option<BString> {
    let log = refs.reflog(&RefName::head()).ok()?;
    let mut seen = 0;
    for entry in log.iter().rev() {
        let Some(rest) = entry.message.strip_prefix(b"checkout: moving from ") else {
            continue;
        };
        seen += 1;
        if seen == n {
            let from = rest.split_str(" to ").next().unwrap_or_default();
            return Some(BString::from(from));
        }
    }
    None
}

/// `@{-N}` on its own: the branch checked out `N` moves ago, as a name.
fn parse_previous(base: &BStr) -> Option<usize> {
    let inner = base.strip_prefix(b"@{-")?.strip_suffix(b"}")?;
    let n: usize = std::str::from_utf8(inner).ok()?.parse().ok()?;
    (n > 0 && inner.iter().all(u8::is_ascii_digit)).then_some(n)
}

/// `<ref>@{N}` and `@{N}`: the value `<ref>` had `N` updates ago, read from its reflog.
///
/// `Ok(None)` means the spec is not of this shape (or names no ref), and the caller carries on.
/// Like Git, asking for the entry just past the oldest returns the oldest entry's *old* value
/// when it is not the null id, and anything further is an error naming the ref as it was typed.
fn resolve_reflog(refs: &RefStore, base: &BStr) -> Result<Option<ObjectId>> {
    let Some(inner) = base.strip_suffix(b"}") else {
        return Ok(None);
    };
    let Some(at) = inner.rfind(b"@{") else {
        return Ok(None);
    };
    let Some(n) = std::str::from_utf8(&inner[at + 2..])
        .ok()
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse::<usize>().ok())
    else {
        return Ok(None);
    };
    let prefix = BStr::new(&inner[..at]);

    let (shown, name) = if prefix.is_empty() {
        match refs.head_branch()? {
            Some(branch) => (BString::from(branch.short_name().as_bytes()), branch),
            None => (BString::from("HEAD"), RefName::head()),
        }
    } else if prefix == "HEAD" {
        (BString::from("HEAD"), RefName::head())
    } else {
        let typed = match parse_previous(prefix) {
            Some(back) => match previous_checkout(refs, back) {
                Some(branch) => branch,
                None => return Ok(None),
            },
            None => prefix.to_owned(),
        };
        match dwim_ref(refs, BStr::new(&typed)) {
            Some(name) => (typed, name),
            None => return Ok(None),
        }
    };

    // `HEAD` without a log of its own reads the log of the branch it names, as Git does.
    let name = if prefix == "HEAD"
        && !crate::refs::reflog::exists(refs.home(&name), &name)
        && let Some(branch) = refs.head_branch()?
    {
        branch
    } else {
        name
    };
    let log = refs.reflog(&name)?;
    if log.is_empty() {
        // No log file at all: not a reflog spec, carry on. A log that exists but holds nothing has
        // only the ref's current value (`@{0}`); anything older is an error, as in Git.
        if !crate::refs::reflog::exists(refs.home(&name), &name) {
            return Ok(None);
        }
        if n == 0 {
            return Ok(refs.resolve_oid(&name).ok());
        }
        return Err(Error::Message(format!(
            "log for {} is empty",
            name.as_bstr()
        )));
    }
    if n < log.len() {
        return Ok(Some(log[log.len() - 1 - n].new));
    }
    if n == log.len() && !log[0].old.is_zero() {
        return Ok(Some(log[0].old));
    }
    Err(Error::Message(format!(
        "log for '{shown}' only has {} entries",
        log.len()
    )))
}

/// Splits the rightmost suffix off a spec, if it has one.
fn split_suffix(spec: &BStr) -> Result<Option<(&BStr, Op)>> {
    let bytes: &[u8] = spec;

    if bytes.last() == Some(&b'}') {
        if let Some(open) = spec.rfind(b"^{") {
            let inner = &spec[open + 2..spec.len() - 1];
            let kind = match inner.as_bytes() {
                b"" => None,
                b"commit" => Some(PeelKind::Commit),
                b"tree" => Some(PeelKind::Tree),
                b"blob" => Some(PeelKind::Blob),
                b"tag" => Some(PeelKind::Tag),
                b"object" => Some(PeelKind::Object),
                _ => return Err(invalid(spec, "unknown peel target")),
            };
            return Ok(Some((&spec[..open], Op::Peel(kind))));
        }
        return Ok(None);
    }

    let last = match bytes.last() {
        Some(byte) => *byte,
        None => return Ok(None),
    };
    if last == b'^' {
        return Ok(Some((&spec[..spec.len() - 1], Op::Parent(1))));
    }
    if last == b'~' {
        return Ok(Some((&spec[..spec.len() - 1], Op::FirstParent(1))));
    }

    if last.is_ascii_digit() {
        // Walk back over the digits to the operator they belong to.
        let mut start = bytes.len();
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start == 0 {
            return Ok(None);
        }
        let count: u32 = match std::str::from_utf8(&bytes[start..])
            .ok()
            .and_then(|text| text.parse().ok())
        {
            Some(count) => count,
            None => return Ok(None),
        };
        let op = match bytes[start - 1] {
            b'^' => Op::Parent(count),
            b'~' => Op::FirstParent(count),
            _ => return Ok(None),
        };
        return Ok(Some((&spec[..start - 1], op)));
    }
    Ok(None)
}

/// Applies one suffix operator.
fn apply(format: ObjectFormat, odb: &Odb, oid: ObjectId, op: Op, spec: &BStr) -> Result<ObjectId> {
    match op {
        Op::Parent(0) => {
            let object = peel_tags(format, odb, oid)?;
            match object.kind() {
                crate::object::ObjectKind::Commit => Ok(oid),
                _ => Err(invalid(spec, "^0 does not name a commit")),
            }
        }
        Op::Parent(n) => {
            let parents = read_parents(format, odb, oid, spec)?;
            let parent = parents
                .get((n - 1) as usize)
                .copied()
                .ok_or_else(|| invalid(spec, "commit has no such parent"))?;
            Ok(parent)
        }
        Op::FirstParent(0) => Ok(oid),
        Op::FirstParent(n) => {
            let mut current = oid;
            for _ in 0..n {
                current = read_parents(format, odb, current, spec)?
                    .first()
                    .copied()
                    .ok_or_else(|| invalid(spec, "ran out of first parents"))?;
            }
            Ok(current)
        }
        Op::Peel(kind) => {
            // `^{tag}` and `^{object}` look at the object as written; everything else peels first.
            if matches!(kind, Some(PeelKind::Tag)) {
                let object = odb.read(&oid)?;
                if object.kind() == crate::object::ObjectKind::Tag {
                    return Ok(oid);
                }
                return Err(invalid(spec, "peeled object is not the requested kind"));
            }
            if matches!(kind, Some(PeelKind::Object)) {
                return Ok(oid);
            }
            let (peeled_id, object) = peel_tags_with_id(format, odb, oid)?;
            let wanted = match kind {
                None => return Ok(peeled_id),
                Some(PeelKind::Commit) => crate::object::ObjectKind::Commit,
                Some(PeelKind::Tree) => crate::object::ObjectKind::Tree,
                Some(PeelKind::Blob) => crate::object::ObjectKind::Blob,
                Some(PeelKind::Tag) | Some(PeelKind::Object) => unreachable!("handled above"),
            };
            if object.kind() == wanted {
                return Ok(peeled_id);
            }
            // A commit is a tree-ish: `REV^{tree}` names the commit's tree, not the commit.
            if wanted == crate::object::ObjectKind::Tree
                && object.kind() == crate::object::ObjectKind::Commit
            {
                return Ok(Commit::parse(format, object.data())?.tree);
            }
            Err(invalid(spec, "peeled object is not the requested kind"))
        }
    }
}

/// Like [`peel_tags`], but also returns the id of the object it ended on.
fn peel_tags_with_id(format: ObjectFormat, odb: &Odb, oid: ObjectId) -> Result<(ObjectId, Object)> {
    let mut current = oid;
    for _ in 0..64 {
        let object = odb.read(&current)?;
        if object.kind() != crate::object::ObjectKind::Tag {
            return Ok((current, object));
        }
        current = Tag::parse(format, object.data())?.object;
    }
    Err(Error::TagCycle { oid: current })
}

/// Follows `tag` objects until something that is not a tag, returning the object read there.
fn peel_tags(format: ObjectFormat, odb: &Odb, oid: ObjectId) -> Result<Object> {
    let mut current = oid;
    // A cycle of tags is corrupt, but a pathological one could be long; Git bounds this too.
    for _ in 0..64 {
        let object = odb.read(&current)?;
        if object.kind() != crate::object::ObjectKind::Tag {
            return Ok(object);
        }
        current = Tag::parse(format, object.data())?.object;
    }
    Err(Error::TagCycle { oid: current })
}

/// Reads `oid` as a commit, returning its parents' ids.
fn read_parents(
    format: ObjectFormat,
    odb: &Odb,
    oid: ObjectId,
    spec: &BStr,
) -> Result<Vec<ObjectId>> {
    let object = peel_tags(format, odb, oid)?;
    if object.kind() != crate::object::ObjectKind::Commit {
        return Err(invalid(spec, "does not name a commit"));
    }
    Ok(Commit::parse(format, object.data())?.parents.clone())
}

/// Whether a spec is entirely hexadecimal, and so worth trying as an id.
fn is_hex(spec: &BStr) -> bool {
    !spec.is_empty() && spec.iter().all(|byte| byte.is_ascii_hexdigit())
}

/// Builds an [`Error::InvalidRevision`] without repeating the struct literal everywhere.
fn invalid(spec: &BStr, reason: &'static str) -> Error {
    Error::InvalidRevision {
        spec: BString::from(spec),
        reason,
    }
}
