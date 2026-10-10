//! Phase 1 — trees: the directory listing that makes a commit a snapshot.
//!
//! A tree body is a flat, concatenated list of entries, each
//!
//! ```text
//! <mode> <name>\0<raw id>
//! ```
//!
//! with the id's width depending on the repository's [`ObjectFormat`] — 20 bytes for SHA-1, 32
//! for SHA-256. There is no count, no length prefix and no terminator: the body ends when the
//! bytes do, and the id's width is the only way to know where one entry stops. A parser that
//! assumes 20 bytes will read a SHA-256 tree as garbage, and the failure looks like corruption
//! rather than like an off-by-twelve.
//!
//! # Sorting is not plain byte order
//!
//! Tree entries are sorted by name, **with directories compared as if their name ended in
//! `/`**. So `foo.txt` sorts before the directory `foo/`, because `.` (0x2E) is less than `/`
//! (0x2F), even though the raw names `foo` and `foo.txt` would order the other way. This single
//! rule is the most common source of trees that Git accepts but that `git fsck` reports as
//! unsorted, and of trees that differ from what `git write-tree` produces for identical input.
//!
//! The rule exists so that a tree can be merged and binary-searched entry-wise without loading
//! it, which is why it is a hard requirement rather than a convention.
//!
//! # Ordering follows the raw bytes
//!
//! Names are compared as **bytes** (`&[u8]`), never as strings. Git does not collate by locale,
//! and a repository can legally contain names that are not UTF-8 — so Rivet compares with
//! `Ord` on byte slices, which is the same total order Git uses.

use bstr::BStr;

use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::hash::ObjectId;
use crate::object::EntryMode;
use crate::object::ObjectKind;

/// Phase 1 — one entry of a tree, borrowed from the tree body it was parsed out of.
///
/// # Caveats
///
/// The `name` is a single path component only — never a path with slashes. Git splits paths at
/// `/` and nests trees one level per component, so a tree entry named `a/b` is malformed and
/// must be rejected (`Error::InvalidTreeEntryName`), not accepted as a nested path.
///
/// The name is stored as bytes, not a string: filenames that are not valid UTF-8 are legal in
/// Git and must survive a round trip through Rivet unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeEntry<'a> {
    /// The entry's mode — file, executable, symlink, subtree or gitlink.
    pub mode: EntryMode,
    /// The single path component, as raw bytes, without a trailing slash.
    pub name: &'a BStr,
    /// The id of the object this entry points at.
    pub id: ObjectId,
}

/// Phase 1 — a borrowed reference to one entry, for lookups that do not copy the name.
///
/// Returned by [`Tree::get`]; the name borrows from the tree body rather than from the entry
/// that was scanned, so the reference stays valid for as long as the tree does.
///
/// # Caveats
///
/// This is a **lookup** result, not a parse result: it carries no id, because the caller asking
/// "is there an entry called `src`" does not always need one and copying an id around in a hot
/// walk is waste. Use [`Tree::iter`] and take the full [`TreeEntry`] when the id is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeEntryRef<'a> {
    /// The entry's mode.
    pub mode: EntryMode,
    /// The single path component, as raw bytes.
    pub name: &'a BStr,
}

/// Phase 1 — a parsed tree, holding a borrowed view of its entries.
///
/// # Caveats
///
/// The entries borrow from the tree **body**, not from a heap-allocated entry list, so this type
/// stores a slice of entries plus the body it came from. The lifetime is on the body: everything
/// reachable from a `Tree<'a>` dies with the buffer that was parsed.
///
/// A tree is a *snapshot*, not a delta. Comparing two trees is therefore a walk of two sorted
/// lists, and because the sort rule puts directories where their `name/` would sort, that walk
/// is a plain merge with no special cases — which is exactly why the rule exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree<'a> {
    /// The raw tree body, kept so entries can borrow from it.
    pub data: &'a [u8],
    /// The parsed entries, in the order they appear in `data`.
    pub entries: Vec<TreeEntry<'a>>,
}

impl<'a> Tree<'a> {
    /// Phase 1 — the object kind of everything this type represents.
    ///
    /// # Caveats
    ///
    /// Always [`ObjectKind::Tree`]. An empty tree is legal and has the well-known id
    /// `4b825dc642cb6eb9a060e54bf8d69288fbee4904` in SHA-1 repositories; it is used by
    /// `git commit` on an empty index and appears in real histories.
    pub const fn kind() -> ObjectKind {
        todo!()
    }

    /// Phase 1 — parse a tree body.
    ///
    /// # Caveats
    ///
    /// `data` is the **body only**, with the `"tree <len>\0"` header already stripped. The
    /// format is needed because the entry id width comes from it and is the only delimiter in
    /// the format.
    ///
    /// Validation is not optional, and every check is a safety check as much as a correctness
    /// one (plan §5):
    ///
    /// * the declared length must be consumed exactly — trailing bytes mean the tree is corrupt,
    ///   and a truncated final entry must be an error, never a partial parse;
    /// * names must not be empty, contain `/` or NUL, or be `.`, `..` or `.git` in any case —
    ///   these are what make path traversal on checkout possible;
    /// * modes must be one of Git's five (`Error::InvalidEntryMode`);
    /// * names must be **unique** (`Error::DuplicateTreeEntry`) — a duplicate makes the tree's
    ///   meaning ambiguous, and `git fsck` rejects it;
    /// * entries must be in canonical order (`Error::UnsortedTree`).
    ///
    /// A tree that fails any of these is not "readable with care": Rivet refuses it, because
    /// every later stage — diff, merge, checkout — assumes these invariants.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedObject`] for a length or structure problem,
    /// [`Error::InvalidEntryMode`], [`Error::DuplicateTreeEntry`], [`Error::InvalidTreeEntryName`]
    /// and [`Error::UnsortedTree`] for the specific violations above.
    pub fn parse(format: ObjectFormat, data: &'a [u8]) -> Result<Self> {
        todo!()
    }

    /// Phase 1 — serialise back to a tree body, ready to be hashed and stored.
    ///
    /// # Caveats
    ///
    /// Returns the **body only** — the caller (or [`crate::object::hash_object`]) prepends the
    /// header. Entries are written in their current order, so the sort rule is the caller's
    /// responsibility here: a builder that appends entries as it finds them must sort before
    /// serialising, or it produces a tree Git will call unsorted.
    pub fn to_bytes(&self) -> Vec<u8> {
        todo!()
    }

    /// Phase 1 — the tree's id under a given object format.
    ///
    /// # Errors
    ///
    /// Only the hasher's collision-detection path.
    pub fn id(&self, format: ObjectFormat) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 1 — the number of entries.
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Phase 1 — whether the tree has no entries.
    ///
    /// # Caveats
    ///
    /// An empty tree is **not** the same as "an empty directory": Git does not track empty
    /// directories at all, and a tree with no entries only appears as the root of an empty
    /// commit or as a legitimate intermediate state during a merge. Do not use this to decide
    /// that a path can be removed from the work tree.
    pub fn is_empty(&self) -> bool {
        todo!()
    }

    /// Phase 1 — iterate the entries in stored order.
    ///
    /// # Caveats
    ///
    /// The iterator's item lifetime is `'a`, the body's, not the tree's own borrow — so entries
    /// can outlive a temporary borrow of the `Tree`. Because the order is canonical, this is safe
    /// to merge-walk against another tree from the same repository.
    pub fn iter(&self) -> std::slice::Iter<'a, TreeEntry<'a>> {
        todo!()
    }

    /// Phase 1 — find an entry by exact name.
    ///
    /// # Caveats
    ///
    /// `name` is a single component, and the comparison is bytewise and case-**sensitive**:
    /// `README` and `readme` are different entries and both are legal in one tree. The
    /// case-insensitive-filesystem problem is real but belongs to checkout, which must detect
    /// that two entries would collide on disk — not here, where the repository's own contents
    /// are what matters.
    ///
    /// A binary search is legitimate because [`Tree::parse`] guarantees sorted order; a tree
    /// that reached this point with unsorted entries would be a bug in the parser, not a case to
    /// handle here.
    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry<'a>> {
        todo!()
    }

    /// Phase 1 — find a subtree entry by name.
    ///
    /// # Caveats
    ///
    /// Finds the entry whose mode is [`EntryMode::Tree`]; a gitlink (`160000`) with the same
    /// name is not a subtree and must not be followed, however directory-shaped it looks. The id
    /// is returned rather than a parsed subtree, because resolving it is a lookup in the object
    /// database and this type has no database.
    pub fn get_subtree(&self, name: &[u8]) -> Option<ObjectId> {
        todo!()
    }

    /// Phase 1 — the id of a path, resolving `/`-separated components recursively.
    ///
    /// # Caveats
    ///
    /// Walks one tree per component, so it needs the object database to fetch each subtree;
    /// the signature takes a lookup closure rather than a database handle so that both the
    /// reader and the test oracle can supply one without this module depending on `odb`.
    ///
    /// An empty path refers to the tree itself. A path that descends through a non-tree entry
    /// (a blob named `src` on the way to `src/lib.rs`) is "not found", not an error — the
    /// distinction matters for `status`, which reports it as an added file.
    ///
    /// # Errors
    ///
    /// Whatever the lookup closure raises — typically
    /// [`crate::error::Error::ObjectNotFound`] for a missing subtree, or
    /// [`crate::error::Error::WrongObjectKind`] if the store returned something that is not a
    /// tree.
    pub fn lookup_path(
        format: ObjectFormat,
        id: ObjectId,
        path: &BStr,
        lookup: &mut dyn FnMut(ObjectId) -> Result<Vec<u8>>,
    ) -> Result<Option<ObjectId>> {
        todo!()
    }
}

/// Phase 1 — build a tree body from a set of entries.
///
/// The write side of [`Tree`]: sorts entries into canonical order (directories compared as if
/// their names ended in `/`), validates the same invariants the parser enforces, and emits the
/// body. Used by `write-tree`, which builds trees bottom-up from the index, and by commit
/// creation.
///
/// # Caveats
///
/// **This function sorts.** Callers that hand it entries in filesystem-walk order get the right
/// answer, which is the point — but callers that have already sorted by raw name (rather than by
/// the directory-adjusted rule) must not assume their order survives, and callers that *need* a
/// particular order (none should) must not use this.
///
/// The recursion is the caller's: this builds one level from entries whose subtree ids have
/// already been computed, because a tree's id depends on its children's ids and cannot be
/// computed in one pass over a flat entry list.
///
/// # Errors
///
/// The same set as [`Tree::parse`] — duplicate names, forbidden names, invalid modes. A builder
/// that validated nothing would produce a tree Rivet's own parser rejects, so the checks are
/// repeated here rather than trusted to the caller.
pub fn write_tree(format: ObjectFormat, entries: &[TreeEntry<'_>]) -> Result<Vec<u8>> {
    todo!()
}

/// Phase 1 — compare two entry names under Git's tree sort rule.
///
/// Returns `Ordering::Equal` only for byte-identical names; otherwise orders by name, except
/// that a name belonging to a directory is compared as though it had a trailing `/` appended.
///
/// # Caveats
///
/// This is the rule the whole module is arranged around, and it is *not* the same as comparing
/// `(name, is_dir)` tuples: `foo.txt` vs directory `foo` compares `.` against the implicit `/`,
/// so `foo.txt` comes first even though `foo` is a prefix of it. Implementations that special-case
/// the prefix relationship get this backwards.
///
/// [`EntryMode`] alone does not determine directory-ness for this purpose — see the note on
/// [`EntryMode::is_tree`] about gitlinks, which are compared as plain names.
pub fn compare_entry_names(
    a_name: &[u8],
    a_mode: EntryMode,
    b_name: &[u8],
    b_mode: EntryMode,
) -> std::cmp::Ordering {
    todo!()
}

/// Phase 1 — validate a single tree entry name.
///
/// Rejects the empty name, `.`, `..`, `.git` in any case, and anything containing `/` or NUL.
///
/// # Caveats
///
/// This is the path-traversal guard, and it is deliberately stricter than "matches Git's
/// writer": on a case-insensitive filesystem a checkout of an entry named `.GIT` would overwrite
/// the repository's own metadata directory, so the comparison is case-insensitive even though
/// the stored name is not. `core.protectHFS` and `core.protectNTFS` add further Windows- and
/// macOS-specific hazards (trailing dots and spaces, `\`, `:`, reserved device names) which must
/// be rejected when those config knobs are on.
///
/// # Errors
///
/// [`Error::InvalidTreeEntryName`], carrying the offending name as bytes so that a non-UTF-8
/// name can still be reported.
pub fn validate_entry_name(name: &[u8]) -> Result<()> {
    todo!()
}
