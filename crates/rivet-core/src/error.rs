//! Phase 1 — the single error type shared by every module in the core.
//!
//! # Design
//!
//! `rivet-core` has exactly one error enum, [`Error`], and one result alias, [`Result`].
//! Later batches **add variants** to [`Error`] rather than introducing their own error types,
//! so a caller can match on any failure in the system without boxing or downcasting.
//!
//! This is deliberately not `anyhow::Error`: the core never erases its error type, because
//! tests and the differential suite match on specific variants. `anyhow` is for the CLI only,
//! where errors are printed rather than handled.
//!
//! # Never panic on repository data
//!
//! No function in this crate may panic on input read from a repository (plan §5). Corrupt
//! objects, truncated packs, bad refs and malformed index files all have a variant here. A
//! panic is only acceptable for a violated *caller* precondition, and those are documented
//! under `# Panics` on the function that has them.

use bstr::BString;

use crate::hash::{ObjectFormat, ObjectId};
use crate::object::ObjectKind;

/// Phase 1 — `Result<T, Error>` for the whole core.
///
/// # Caveats
///
/// This alias has a single type parameter. A `Result<T, E>` with any other error type must be
/// written out in full; do not add a second parameter to this alias, because it is used
/// unqualified in every module and the signature churn would be large.
pub type Result<T> = std::result::Result<T, Error>;

/// Phase 1 — every failure that `rivet-core` can report.
///
/// Variants are grouped by the module that first raises them. A later batch that needs a new
/// failure mode adds a variant here, in the matching group.
///
/// # Caveats
///
/// This enum is `#[non_exhaustive]`: the CLI (and any downstream crate) must include a
/// wildcard arm when matching. Variants can be *constructed* from outside the crate, only
/// exhaustive matching is prevented.
///
/// Error messages are human-facing text and are not stable; do not parse them. Code that needs
/// to distinguish cases matches on the variant, never on the rendered string.
///
/// # Errors
///
/// Not applicable — this is the error type itself.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    // ── plumbing: I/O, paths, repository layout ───────────────────────────────────────────
    /// A filesystem or network operation failed, with no repository path available to attach.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A filesystem operation failed on a specific file.
    ///
    /// The path is kept as bytes, because a path that failed to be a valid path may well be a
    /// path that is not valid UTF-8 — reporting it must not require lossy conversion first.
    #[error("i/o error on {path}: {source}")]
    IoAtPath {
        /// The path that failed, repository-relative or absolute as appropriate.
        path: BString,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// No `.git` directory was found here or in any parent directory.
    #[error("not a git repository (or any parent up to the filesystem root): {0}")]
    NotARepository(BString),

    /// `init` was asked to create a repository where one already exists and no `--force` was
    /// given. Re-initialising an existing repository is legal in Git; this error is only for
    /// the case Rivet must refuse.
    #[error("a repository already exists at {0}")]
    RepositoryExists(BString),

    /// The operation needs a work tree, but the repository is bare.
    #[error("{op} requires a work tree, but {path} is a bare repository")]
    BareRepository {
        /// The repository directory.
        path: BString,
        /// The operation that was refused, for the message.
        op: &'static str,
    },

    /// A path is syntactically unusable as a repository path.
    ///
    /// This covers the checkout-safety cases: absolute paths, `..` components, and paths that
    /// escape the work tree once normalised. It also covers Windows-only hazards (`\`, `:`,
    /// reserved device names, trailing dots/spaces) which must be rejected when
    /// `core.protectNTFS` semantics apply.
    #[error("path escapes the repository root: {0}")]
    InvalidPath(BString),

    /// Text that must be UTF-8 was not.
    ///
    /// `what` names the field, e.g. `"config key"`. Note that this is almost never the right
    /// error for repository *content* — paths and messages are bytes on purpose. It is for
    /// places where Git itself requires UTF-8, such as config section names.
    #[error("invalid utf-8 in {what}")]
    InvalidUtf8 {
        /// What was being decoded.
        what: &'static str,
    },

    /// `core.repositoryformatversion` is newer than Rivet understands.
    ///
    /// Git reserves the right to bump this; a future version may change the on-disk layout in
    /// ways that make guessing dangerous. Refuse rather than proceed.
    #[error("unsupported repository format version {version} (rivet supports 0 and 1)")]
    UnsupportedRepositoryFormatVersion {
        /// The value found in the config.
        version: u32,
    },

    /// `extensions.objectFormat` holds a value that is not `sha1` or `sha256`.
    #[error("unknown object format {0} (expected \"sha1\" or \"sha256\")")]
    UnknownObjectFormat(BString),

    /// Two repositories (or a repository and a remote) use different object formats.
    ///
    /// Per plan §2.1 rule 5, Rivet refuses to fetch or push between mismatched formats rather
    /// than attempting a SHA-1 ↔ SHA-256 conversion that Git has not finished either. The
    /// mismatch is detected from the `object-format` capability in the protocol v2 handshake,
    /// so the refusal happens before any objects are exchanged.
    #[error("object format mismatch: this repository is {expected:?}, the other side is {found:?}")]
    ObjectFormatMismatch {
        /// The local repository's format.
        expected: ObjectFormat,
        /// The format the peer announced.
        found: ObjectFormat,
    },

    /// A `.lock` file is held by another process, so the update cannot proceed.
    ///
    /// Git's locking is advisory and file-based, so a *stale* lock left by a crashed process
    /// looks identical to a live one. Rivet must not delete the lock automatically; the
    /// message should tell the user which file to remove.
    #[error("could not take lock {path}: another process holds it")]
    LockHeld {
        /// The lock file that could not be created.
        path: BString,
    },

    // ── hashing ───────────────────────────────────────────────────────────────────────────
    /// A raw byte slice is not the right length for any object id.
    #[error("invalid object id: expected {expected_len} bytes, got {found_len}")]
    InvalidHash {
        /// The length that was required.
        expected_len: usize,
        /// The length that was supplied.
        found_len: usize,
    },

    /// A hex string is not a valid object id: wrong length, or non-hex characters.
    #[error("invalid hex object id {value}: {reason}")]
    InvalidHex {
        /// The offending input, as bytes — it may not be valid UTF-8.
        value: BString,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// A short id matched more than one object.
    ///
    /// Git's shortest-unique-prefix rule means an abbreviation is only valid while it stays
    /// unique; a repository that grows can turn a previously-unique prefix ambiguous. Report
    /// the candidates so the user can be more specific.
    #[error("ambiguous object id prefix {prefix}: {candidates} candidates")]
    AmbiguousPrefix {
        /// The abbreviation that was supplied.
        prefix: BString,
        /// How many objects it matched. Zero matches is `ObjectNotFound`, not this.
        candidates: usize,
    },

    /// Content did not hash to the id it was stored under.
    ///
    /// This is the strongest corruption signal Rivet has. It must be checked on **every**
    /// object read, from loose objects and from packs alike, before the payload is trusted.
    #[error("object integrity check failed: content hashes to {actual}, expected {expected}")]
    HashMismatch {
        /// The id the object was requested or indexed under.
        expected: ObjectId,
        /// The id its actual content produces.
        actual: ObjectId,
    },

    // ── objects ───────────────────────────────────────────────────────────────────────────
    /// No object with this id is present in the object database.
    ///
    /// For a request by abbreviation this also covers "the prefix matched nothing".
    #[error("object not found: {oid}")]
    ObjectNotFound {
        /// The id that was requested.
        oid: ObjectId,
    },

    /// An object was read successfully but is not the kind the caller expected.
    ///
    /// Finding a blob where a commit was expected usually means a broken ref or a truncated
    /// history, so this is reported rather than treated as "not found".
    #[error("expected a {expected} object, found a {found}")]
    WrongObjectKind {
        /// The id, when the caller knows it. `None` for a loose object whose filename was
        /// inconsistent, where there is no trustworthy id to report.
        oid: Option<ObjectId>,
        /// What the caller needed.
        expected: ObjectKind,
        /// What was actually there.
        found: ObjectKind,
    },

    /// The bytes stored under an id do not decode as that object.
    ///
    /// `reason` is a short human-readable diagnosis (`"truncated tree entry"`,
    /// `"bad zlib stream"`, …). Bad zlib is included here: the decompressed length is known
    /// from the object header, so a stream that does not produce exactly that many bytes is
    /// corruption, not a short read.
    #[error("corrupt object {oid}: {reason}")]
    CorruptObject {
        /// The id the corrupt bytes were stored under.
        oid: ObjectId,
        /// What was wrong with them.
        reason: String,
    },

    /// A payload does not decode as the object kind it was labelled with.
    ///
    /// Distinct from [`Error::CorruptObject`]: there the *storage* is suspect, here the bytes
    /// are intact but do not follow the grammar for that kind.
    #[error("malformed {kind} object: {reason}")]
    MalformedObject {
        /// The kind that was being parsed.
        kind: ObjectKind,
        /// What was wrong with it.
        reason: String,
    },

    /// A required header is missing from a commit or tag object.
    #[error("{kind} object has no `{header}` header")]
    MissingHeader {
        /// The object kind being parsed.
        kind: ObjectKind,
        /// The header name, e.g. `"tree"`.
        header: &'static str,
    },

    /// A required header is present but does not parse.
    ///
    /// For example a commit `tree` line whose value is not a valid object id for the
    /// repository's format.
    #[error("{kind} object has a malformed `{header}` header: {reason}")]
    InvalidHeader {
        /// The object kind being parsed.
        kind: ObjectKind,
        /// The header name.
        header: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// Two entries in one tree have the same name.
    ///
    /// `git fsck` reports this and so must Rivet: a duplicate name makes the tree's meaning
    /// ambiguous and would let a checkout write one entry and report another.
    #[error("tree contains two entries named {name}")]
    DuplicateTreeEntry {
        /// The duplicated name.
        name: BString,
    },

    /// A tree entry's mode is not one of the five Git allows.
    #[error("tree entry {name} has invalid mode {mode:o}")]
    InvalidEntryMode {
        /// The entry whose mode is bad.
        name: BString,
        /// The raw mode, in octal — which is how it appears in the tree bytes.
        mode: u32,
    },

    /// A tree entry name is one Git forbids.
    ///
    /// Rejected: the empty name, `.`, `..`, `.git` (in any case, to match
    /// `core.protectHFS`/`protectNTFS` behaviour on case-insensitive filesystems), and any
    /// name containing `/` or NUL. This is a **security** check as much as a correctness one:
    /// a `.git` entry written to disk on checkout would let a repository rewrite its own
    /// configuration, and `..` would escape the work tree.
    #[error("tree entry name {name} is not allowed")]
    InvalidTreeEntryName {
        /// The offending name.
        name: BString,
    },

    /// Tree entries are not in Git's canonical sorted order.
    #[error("tree entry {name} follows {previous}, which is out of order")]
    UnsortedTree {
        /// The entry that is out of place.
        name: BString,
        /// The entry immediately before it.
        previous: BString,
    },

    /// A tag chain loops back on itself.
    ///
    /// Nested tags are legal (`tag -> tag -> commit`), so peeling must bound its walk rather
    /// than trusting the data to terminate.
    #[error("tag {oid} forms a cycle")]
    TagCycle {
        /// The tag object the cycle was detected at.
        oid: ObjectId,
    },

    // ── packs and the object database ─────────────────────────────────────────────────────
    /// A packfile does not decode.
    #[error("corrupt pack: {reason}")]
    CorruptPack {
        /// What was wrong with it.
        reason: String,
    },

    /// A pack index (`.idx`) does not decode.
    #[error("corrupt pack index: {reason}")]
    CorruptPackIndex {
        /// What was wrong with it.
        reason: String,
    },

    /// The packfile version is not one Rivet reads.
    #[error("unsupported pack version {version} (rivet reads v2 and v3)")]
    UnsupportedPackVersion {
        /// The version from the pack header.
        version: u32,
    },

    /// The `.idx` version is not one Rivet reads.
    #[error("unsupported pack index version {version} (rivet reads v2)")]
    UnsupportedPackIndexVersion {
        /// The version from the index header.
        version: u32,
    },

    /// A delta references a base object that is not in the pack.
    #[error("delta base {base} for object {oid} is missing")]
    MissingDeltaBase {
        /// The object whose delta could not be applied.
        oid: ObjectId,
        /// The base the delta names.
        base: ObjectId,
    },

    /// A received pack is thin: it deltifies against objects the sender assumed we have.
    ///
    /// Fetch and push both receive thin packs by design. Rivet must complete them locally
    /// before indexing — `index-pack --fix-thin` — rather than treating this as corruption.
    #[error("thin pack: base {base} for object {oid} was not sent")]
    ThinPack {
        /// The deltified object.
        oid: ObjectId,
        /// The absent base.
        base: ObjectId,
    },

    /// Bytes remain after the last complete object in a pack, or a reader overran it.
    #[error("pack object at offset {offset} left {remaining} bytes unread")]
    TrailingPackBytes {
        /// The offset the reader stopped at.
        offset: u64,
        /// How many bytes were unexpected.
        remaining: usize,
    },

    /// A delta does not decode, or does not apply to the base it names.
    ///
    /// Raised by [`crate::pack::apply_delta`] and by the delta-chain resolver. This is its own
    /// variant rather than a [`Error::CorruptPack`] carrying a free-text reason because the same
    /// code runs over bytes that are not a pack at all — a thin pack's missing bases are applied
    /// from the object database, and `index-pack` reads a pack from a socket — and because a
    /// caller (and the fuzzer, plan §6) needs to tell "the delta instructions are bad" apart from
    /// "the container is bad".
    ///
    /// The reason covers a base length that disagrees with the delta, a truncated or over-long
    /// instruction stream, an unknown opcode, a copy that reaches outside the base, and a result
    /// whose length is not the one the delta declared.
    #[error("corrupt delta: {reason}")]
    CorruptDelta {
        /// What was wrong with it.
        reason: String,
    },

    /// A delta chain is deeper than Rivet will follow.
    ///
    /// `OFS_DELTA` bases always precede the entries that depend on them, so those chains
    /// terminate by construction; `REF_DELTA` bases are named by object id and may name *each
    /// other*, so a corrupt or hostile pack can describe a chain that never ends. The resolver
    /// stops at [`crate::pack::MAX_DELTA_DEPTH`] instead of recursing until the stack overflows
    /// (plan §5) — this is the error it reports when it does.
    ///
    /// Reported rather than silently truncated: an object whose deltas were cut short would
    /// otherwise be hashed and written, producing a repository that passes `git fsck` while
    /// containing an object whose content is not what its id says.
    #[error("delta chain for {oid} is deeper than {depth} levels")]
    DeltaChainTooDeep {
        /// The object whose chain was too long.
        oid: ObjectId,
        /// The limit that was hit, i.e. `pack::MAX_DELTA_DEPTH`.
        depth: usize,
    },

    // ── refs ──────────────────────────────────────────────────────────────────────────────
    /// A ref name violates `git check-ref-format`.
    #[error("invalid ref name {name}: {reason}")]
    InvalidRefName {
        /// The offending name.
        name: BString,
        /// Which rule it broke.
        reason: &'static str,
    },

    /// The named ref does not exist.
    #[error("ref not found: {name}")]
    RefNotFound {
        /// The full ref name, e.g. `refs/heads/main`.
        name: BString,
    },

    /// An operation that requires a direct ref was given a symbolic one, or vice versa.
    #[error("not a valid operation for a symbolic ref: {name}")]
    SymbolicRef {
        /// The ref in question.
        name: BString,
    },

    /// The operation needs a commit, but `HEAD` is detached.
    ///
    /// Callers that can work detached (log, checkout) must not raise this; only operations
    /// that genuinely need a branch, such as `branch` with no arguments on a detached head.
    #[error("HEAD is detached")]
    DetachedHead,

    /// A ref exists but does not (ultimately) resolve to a commit.
    ///
    /// Annotated tags must be peeled first; this is the error when peeling ends somewhere
    /// other than a commit, or when the chain is too deep.
    #[error("{name} does not point at a commit")]
    NotACommit {
        /// The ref that could not be peeled to a commit.
        name: BString,
    },

    /// A symbolic ref chain is longer than Rivet will follow, or loops back on itself.
    ///
    /// `HEAD` pointing at `refs/heads/main`, which is itself a symbolic ref pointing at another
    /// ref, is legal and ordinary for a repository mid-`switch`. What is not legal is a chain
    /// that never terminates: a ref pointing at itself, or two refs pointing at each other.
    /// Git bounds the walk at five hops (`SYMREF_MAXDEPTH`) and so does Rivet, because the
    /// alternative is recursing until the stack overflows on a hostile or corrupt repository
    /// (plan §5).
    ///
    /// Reported for the depth case as well as the cycle case: a chain deeper than the limit is
    /// refused identically whether or not it would eventually terminate, because a repository
    /// that Git cannot resolve is not one Rivet should guess at.
    #[error("symbolic ref {name} is nested more than {limit} deep, or forms a cycle")]
    SymbolicRefCycle {
        /// The ref whose resolution ran out of budget.
        name: BString,
        /// The hop limit that was hit, i.e. `refs::MAX_SYMBOLIC_DEPTH`.
        limit: usize,
    },

    /// `packed-refs` does not decode.
    ///
    /// The file has no checksum and no length prefixes, so a truncated or hand-edited
    /// `packed-refs` is only detectable line by line. A line that is neither a comment, a
    /// blank, a `^`-prefixed peel, nor `<id> <refname>` stops the parse rather than being
    /// skipped: silently dropping a ref would make a branch disappear, and the next `gc` would
    /// then write a `packed-refs` that no longer contains it.
    #[error("corrupt packed-refs at line {line}: {reason}")]
    CorruptPackedRefs {
        /// 1-based line number in the file, counting from the start.
        line: usize,
        /// What was wrong with that line.
        reason: &'static str,
    },

    /// A reflog file does not decode.
    ///
    /// Two failures hide here: a line whose `<old> <new>` are not object ids of the repository's
    /// format, and a signature that does not parse. A reflog is *not* authoritative — it records
    /// history, not state — but a corrupt log is still reported rather than truncated, because
    /// `rivet reflog` and `HEAD@{n}` revision syntax would otherwise silently address the wrong
    /// commit.
    #[error("corrupt reflog {path} at line {line}: {reason}")]
    CorruptReflog {
        /// The reflog file, relative to the git directory (e.g. `logs/HEAD`).
        path: BString,
        /// 1-based line number in the file.
        line: usize,
        /// What was wrong with that line.
        reason: &'static str,
    },

    // ── index ─────────────────────────────────────────────────────────────────────────────
    /// The index file does not start with `DIRC`.
    #[error("index has bad signature {found:?} (expected \"DIRC\")")]
    BadIndexSignature {
        /// The four bytes that were found instead.
        found: [u8; 4],
    },

    /// The index version is not one Rivet reads.
    ///
    /// Rivet handles v2 and v3. v4 (prefix-compressed paths) and the split-index extension are
    /// out of scope for v1 and must be refused explicitly rather than misparsed.
    #[error("unsupported index version {version} (rivet reads v2 and v3)")]
    UnsupportedIndexVersion {
        /// The version from the index header.
        version: u32,
    },

    /// The index's trailing checksum does not match its contents.
    ///
    /// A mismatch means a writer was interrupted or another process is mid-write; the index
    /// must be treated as unusable rather than partially trusted.
    #[error("index checksum mismatch: stored {expected}, computed {actual}")]
    BadIndexChecksum {
        /// The checksum stored in the file.
        expected: ObjectId,
        /// The checksum Rivet computed over the preceding bytes.
        actual: ObjectId,
    },

    /// An index entry's path is too long to be stored.
    #[error("index entry path is too long: {path}")]
    IndexEntryTooLong {
        /// The offending path.
        path: BString,
    },

    /// An index entry's stage bits are outside 0–3.
    #[error("index entry has invalid stage {stage} (expected 0-3)")]
    InvalidStage {
        /// The raw stage bits.
        stage: u8,
    },

    /// A path has conflict stages instead of a single resolved entry.
    ///
    /// Git records an in-progress merge by storing up to three entries (stages 1/2/3) for the
    /// same path. Committing refuses while any path is unmerged, exactly as Git does.
    #[error("path is unmerged: {path}")]
    UnmergedPath {
        /// The conflicted path.
        path: BString,
    },

    /// The index file does not decode.
    ///
    /// Distinct from [`Error::BadIndexSignature`], [`Error::UnsupportedIndexVersion`] and
    /// [`Error::BadIndexChecksum`], which each name one specific failure. This one covers
    /// everything else: a file too short for its own header, an entry list that runs off the
    /// end, a path that overruns the entry it belongs to, and padding that is not NUL. A
    /// truncated index is the normal result of a writer being killed, so this is a case that
    /// happens in practice rather than one that only fuzzing reaches.
    #[error("corrupt index at offset {offset}: {reason}")]
    CorruptIndex {
        /// Byte offset in the index file where the problem was found.
        offset: usize,
        /// What was wrong there.
        reason: &'static str,
    },

    // ── config ────────────────────────────────────────────────────────────────────────────
    /// A config file does not follow the Git INI grammar.
    ///
    /// Covers a section header that is never closed, a section or key name with a character Git
    /// forbids, a bare key outside any section, and a line that is neither a comment, a blank,
    /// a section header, nor a `key = value` pair.
    ///
    /// Git itself is lenient about several of these (it warns and continues on some, and its
    /// parser has historically accepted a stray `[` mid-file); where Rivet and Git disagree the
    /// behaviour must be relaxed to match Git, never the other way round, because refusing a
    /// config Git accepts makes a repository unopenable.
    #[error("bad config at line {line}: {reason}")]
    ConfigSyntax {
        /// 1-based line number in the file being parsed.
        line: usize,
        /// Which rule the line broke.
        reason: &'static str,
    },

    /// A config value is not of the type that was asked for.
    ///
    /// Raised by the typed accessors. It carries both the rendered `section.key` (or
    /// `section.subsection.key`) and the offending value, because the user's next move is to
    /// find the line, and the value is what identifies it.
    ///
    /// Retrieving a key that is simply **absent** is not this error: the typed accessors return
    /// `Ok(None)` for a missing key and let the caller apply a default, which is what Git's
    /// `git_config_get_*` does. This is only for a key that is present and wrong.
    #[error("bad value for {key}: {value} is not {expected}")]
    ConfigInvalidValue {
        /// The key as the user would write it, e.g. `core.filemode` or `remote.origin.url`.
        key: BString,
        /// The value that did not parse.
        value: BString,
        /// What was expected, e.g. `"a boolean"` or `"an integer"`.
        expected: &'static str,
    },

    // ── work tree ─────────────────────────────────────────────────────────────────────────
    /// Checkout or switch would destroy local modifications.
    ///
    /// This is the safety rule that the tinyvcs `victim.txt` bug violated: before touching the
    /// work tree, compute the paths that differ between current and target, and refuse if any
    /// of them differs between work tree and index, *or* between index and `HEAD`. The second
    /// clause is what catches "modified in the work tree, deleted in the target".
    #[error("refusing to overwrite local changes in: {paths:?}")]
    DirtyWorktree {
        /// Every path that would be lost. Report all of them, not just the first.
        paths: Vec<BString>,
    },

    /// A path was given that lies outside the work tree.
    #[error("path is outside the work tree: {0}")]
    OutsideWorktree(BString),

    /// A path is excluded by an ignore rule.
    ///
    /// # Caveats
    ///
    /// The field holding the rule's origin is called `rule`, not `source`, because `source` is
    /// reserved by `thiserror` for the error that caused this one. Renaming it back would make
    /// this variant fail to derive.
    #[error("{path} is ignored by {rule}")]
    PathIgnored {
        /// The ignored path.
        path: BString,
        /// Where the rule came from, e.g. `.gitignore:12` — used by `check-ignore -v`.
        rule: BString,
    },

    /// An ignore pattern could not be parsed.
    ///
    /// Rare by design. Git's `.gitignore` grammar treats almost every byte sequence as a legal
    /// pattern — a trailing backslash, an unterminated `[` class and a lone `!` all have defined
    /// (if surprising) meanings, and Git never rejects them. This variant is for the cases where
    /// matching would be *undefined* rather than merely odd: a pattern consisting only of `/`
    /// characters after anchoring and trailing-slash removal, which matches no path at all and
    /// would therefore be a silently dead rule.
    ///
    /// Note that a pattern Git would treat as an error comment is **not** an error here — blank
    /// lines and lines whose first non-escaped character is `#` are comments, and
    /// [`crate::ignore::Pattern::parse`] reports them as `Ok(None)`, not as a failure.
    #[error("invalid ignore pattern {pattern}: {reason}")]
    InvalidIgnorePattern {
        /// The line as it appeared in the source, with any `!` or trailing `/` still on it.
        pattern: BString,
        /// Which rule it broke.
        reason: &'static str,
    },

    /// Checkout would overwrite an untracked file.
    ///
    /// An untracked file is not recoverable from the object store, so this always refuses;
    /// there is no `--force` that makes losing it safe.
    #[error("untracked working tree file would be overwritten: {0}")]
    UntrackedOverwrite(BString),

    /// A path was named that does not exist in the relevant tree, index or work tree.
    #[error("pathspec '{0}' did not match any files")]
    PathNotFound(BString),

    // ── diff and merge ────────────────────────────────────────────────────────────────────
    /// The two commits have no common ancestor.
    #[error("no merge base between {ours} and {theirs}")]
    NoMergeBase {
        /// Our commit.
        ours: ObjectId,
        /// Their commit.
        theirs: ObjectId,
    },

    /// A merge is already in progress (`MERGE_HEAD` exists).
    #[error("a merge is already in progress")]
    MergeInProgress,

    /// `merge --abort` or `merge --continue` was run with no merge in progress.
    #[error("no merge in progress")]
    NoMergeInProgress,

    /// A merge finished with conflicts, which are left in the index and work tree.
    ///
    /// Not a failure to roll back: the conflicted state is the intended output, matching Git,
    /// so that `git status`, `git mergetool` and `git merge --abort` all work on it.
    #[error("merge left conflicts in: {paths:?}")]
    MergeConflicts {
        /// The conflicted paths, with conflict markers written to the work tree.
        paths: Vec<BString>,
    },

    // ── revision parsing ──────────────────────────────────────────────────────────────────
    /// A revision specification does not parse.
    ///
    /// Raised by [`crate::revwalk::RevSpec::parse`] and by `rev-parse` for anything that is not a
    /// well-formed name: a suffix on something that is not a commit (`^{tree}` on a blob),
    /// `~` or `^` followed by a non-number, a range with a missing side, an `@{n}` whose `n` is
    /// not a number. The spec is kept as bytes because it came from a command line or, via a
    /// `MERGE_HEAD`-style file, from disk.
    ///
    /// This is deliberately distinct from [`Error::ObjectNotFound`] and
    /// [`Error::RefNotFound`]: "I cannot parse what you typed" and "what you typed names
    /// nothing" are different problems for the user, and Git's own `rev-parse` distinguishes
    /// them too.
    #[error("invalid revision {spec}: {reason}")]
    InvalidRevision {
        /// The specification as it was written.
        spec: BString,
        /// Which rule it broke.
        reason: &'static str,
    },

    // ── remotes ───────────────────────────────────────────────────────────────────────────
    /// A remote URL does not parse, or names a transport Rivet does not speak.
    ///
    /// Raised by [`crate::remote::RemoteUrl::parse`]. `reason` is a short static diagnosis
    /// (`"unsupported scheme"`, `"no host"`, `"empty path"`), because the set of ways a URL can be
    /// wrong is closed and the caller cannot do anything different per case — unlike
    /// [`Error::Protocol`], where the peer is untrusted and the text is arbitrary.
    ///
    /// The URL is stored **as the user wrote it**, credentials included, because this error is
    /// raised before the URL has been decomposed far enough to strip them. A caller reporting it
    /// must print [`crate::remote::RemoteUrl::redacted`] or run the raw string through
    /// [`crate::remote::redact_url`] — see the note on [`Error::Authentication`] about never
    /// logging a secret. This is the one error variant where that warning is easy to forget,
    /// because nothing about the type says the value might contain a password.
    #[error("invalid remote url {url}: {reason}")]
    InvalidRemoteUrl {
        /// The URL as supplied, credentials and all.
        url: BString,
        /// Which rule it broke.
        reason: &'static str,
    },

    /// The HTTP server answered with a status that is not the one a smart-HTTP endpoint returns.
    ///
    /// Separate from [`Error::Transport`] because the status is the diagnosis: a `404` means the
    /// URL is wrong or the server does not serve `git-http-backend` at it, a `403` means the
    /// repository exists and the caller may not read it, and a `5xx` means trying again may work.
    /// Collapsing them into a free-text transport error makes all three look like a network fault.
    ///
    /// `401` and `407` never reach here — they are [`Error::Authentication`], because the caller's
    /// next move is to supply credentials rather than to look at the URL.
    #[error("{url} returned HTTP {status}")]
    HttpStatus {
        /// The URL that was requested, with credentials stripped.
        url: BString,
        /// The status code the server returned.
        status: u16,
    },

    /// The transport itself failed: DNS, TCP, TLS, a missing `ssh`.
    #[error("transport error: {0}")]
    Transport(String),

    /// The peer sent something that violates the Git wire protocol.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Credentials were missing or rejected.
    ///
    /// Never include the credential itself in this error, or in any log line — the plan calls
    /// this out explicitly. `reason` describes the rejection, not the secret.
    #[error("authentication failed for {url}: {reason}")]
    Authentication {
        /// The remote URL, with any embedded userinfo stripped.
        url: BString,
        /// Why authentication failed.
        reason: String,
    },

    /// The remote refused a ref update.
    #[error("remote rejected {name}: {reason}")]
    RefUpdateRejected {
        /// The ref the remote refused to move.
        name: BString,
        /// The remote's explanation, from `report-status`.
        reason: String,
    },

    /// A push would move a ref backwards over commits the remote has.
    ///
    /// Only `--force` (or an explicit `+` on the refspec) may override this. The check must
    /// happen locally so the user gets a clear error, but the remote re-checks it: the local
    /// check is a courtesy, not a guarantee, because the remote may have moved since the last
    /// fetch.
    #[error("non-fast-forward update to {name}: {old} -> {new}")]
    NonFastForward {
        /// The ref being pushed.
        name: BString,
        /// The value the remote reported.
        old: ObjectId,
        /// The value Rivet wants to set.
        new: ObjectId,
    },

    /// A named remote is not configured.
    #[error("no such remote: {0}")]
    NoSuchRemote(BString),

    // ── generic ───────────────────────────────────────────────────────────────────────────
    /// A human-facing message with no more specific variant.
    ///
    /// A last resort: prefer adding a variant, because a caller cannot match on this.
    #[error("{0}")]
    Message(String),

    /// A path in the scaffold that has no implementation yet.
    #[error("{what} is not implemented yet")]
    NotImplemented {
        /// The unimplemented operation.
        what: &'static str,
    },
}
