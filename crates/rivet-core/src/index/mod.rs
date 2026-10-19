//! Phase 3 — the index: `.git/index`, Git's staging area, in DIRC v2 (and v3).
//!
//! The index is the one file that makes Rivet a VCS rather than a snapshot tool. tinyvcs recorded
//! a whole tree per commit; Git records what the *user chose*, and the index is where that choice
//! lives. It is also the only file in a Git repository that is simultaneously a sorted database, a
//! stat cache and a workspace for merge conflicts.
//!
//! # The layout
//!
//! ```text
//! "DIRC"                     4 bytes, literal
//! version                    4 bytes big-endian: 2, 3, or 4
//! entry count                4 bytes big-endian
//! entry 0                    see below, 8-byte aligned
//! entry 1
//! …
//! extension 0                optional, each: 4-byte signature, 4-byte length, payload
//! …
//! checksum                   at the repository's id width: over everything above
//! ```
//!
//! An entry is:
//!
//! ```text
//! ctime seconds              4 bytes
//! ctime nanoseconds          4 bytes
//! mtime seconds              4 bytes
//! mtime nanoseconds          4 bytes
//! dev                        4 bytes
//! ino                        4 bytes
//! mode                       4 bytes
//! uid                        4 bytes
//! gid                        4 bytes
//! file size                  4 bytes
//! object id                  `format.len()` bytes — 20 for SHA-1, 32 for SHA-256
//! flags                      2 bytes
//! [extended flags]           2 bytes, v3 only
//! path                       NUL-terminated, padded to a multiple of 8 from the entry's start
//! ```
//!
//! # Caveats that bite
//!
//! * **The id width is not in the file.** There is no length field and no version that implies one;
//!   the entry is 20 bytes longer in a SHA-256 repository and the only way to know is
//!   `extensions.objectFormat`. Parsing a SHA-256 index with a SHA-1 reader does not fail — it
//!   reads a plausible entry count, then walks off into the path bytes and produces garbage paths,
//!   which is why every entry point here takes an [`ObjectFormat`]. This is the same trap as trees
//!   and packs (plan §2.1).
//! * **Entries are sorted and must stay sorted.** The order is `memcmp` on the raw path bytes,
//!   with ties broken by stage ascending — stage 1, 2, 3 for one path, which is how a conflict is
//!   stored. Git binary-searches this file, so an unsorted index is not merely untidy: `git add`
//!   will fail to find an entry that is present.
//! * **The path is compared as bytes, and `/` is not special.** `foo.txt` sorts before `foo/bar`
//!   because `.` (0x2E) is below `/` (0x2F), which is the opposite of what a directory-oriented
//!   sort gives — and getting it wrong produces an index Git rejects as unsorted.
//! * **Entries are 8-byte aligned from the start of the entry, not the start of the file.** The
//!   padding is *inside* the entry — between the path's NUL and the next entry — so a reader that
//!   aligns from the file position after the count gets every entry after the first one wrong when
//!   an entry's natural length is not a multiple of eight.
//! * **The flags' path length is a lie for long paths.** The 12-bit field holds the path's length
//!   *including* the NUL, and saturates at 0xFFF; a longer path has 0xFFF there and its true length
//!   must be found by scanning for the NUL. Trusting the field truncates every deep path. The
//!   saturating value also means the field cannot be used to distinguish a 4095-byte path from a
//!   4096-byte one.
//! * **Extensions are opaque and must round-trip.** `TREE` caches the tree a `commit` will write,
//!   `REUC` records pre-merge state, `UNTR` caches untracked-directory scans, and every Git version
//!   adds more. A reader that drops what it does not understand makes the next `git status`
//!   slower or, for `TREE`, subtly wrong. **Optional** extensions have a lowercase first
//!   signature byte and may be dropped; mandatory ones are uppercase and must be refused if
//!   unknown — the one place in this file where the signature's case carries meaning.
//! * **The trailing checksum is over everything before it**, at the repository's width, and it is
//!   what makes a truncated or half-written index detectable. A mismatch is
//!   [`crate::error::Error::BadIndexChecksum`] and the index must then be treated as unusable
//!   rather than partially trusted — a concurrent `git add` mid-write is the common cause, and
//!   reading through it produces a mix of two states.
//! * **v4 is refused, not misread.** v4 prefix-compresses paths (removing the common prefix with
//!   the previous entry) and drops the padding; a v2 reader fed a v4 file produces paths that look
//!   like fragments. Plan §1 puts v4 and the split index out of scope, so
//!   [`crate::error::Error::UnsupportedIndexVersion`] is the response.
//! * **The stat data is a cache, not truth.** An entry's cached mtime and size are a *hint* that
//!   the work tree file has not changed. Git's racy-git rule exists because the hint is unsound on
//!   its own: if a file's mtime is not strictly older than the index's own mtime, the file may have
//!   been modified within the same clock tick that wrote the index, and its content **must** be
//!   re-hashed rather than trusted. See [`Index::is_racy`].
//! * **The index is not in the object database.** It borrows object ids from it, but an id in the
//!   index need not name an object that exists — a `git reset` after a `gc` can leave ids that are
//!   gone — and nothing here reads the object store.

use std::cmp::Ordering;
use std::path::Path;

use bstr::{BStr, BString};

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{EntryMode, ObjectKind, hash_object};

/// The largest path length the 12-bit name-length field can express, and therefore the longest
/// path `parse` and `to_bytes` accept. Longer names are refused rather than truncated: a saturated
/// length field no longer describes the entry, so another reader could not recover the name.
const INDEX_PATH_MAX: usize = 0x0FFF;

/// Map a stored index mode word to its [`EntryMode`].
///
/// The index stores the full numeric mode with its type bits (`100644`, `120000`, `160000`,
/// `040000`), so this matches the canonical five exactly instead of masking down to permissions the
/// way [`EntryMode::from_file_mode`] does for a `stat` result.
fn entry_mode_from_u32(mode: u32) -> Option<EntryMode> {
    match mode {
        0o040000 => Some(EntryMode::Tree),
        0o100644 => Some(EntryMode::Blob),
        0o100755 => Some(EntryMode::BlobExecutable),
        0o120000 => Some(EntryMode::Link),
        0o160000 => Some(EntryMode::Commit),
        _ => None,
    }
}

/// Phase 3 — the name of the index file inside a git directory.
///
/// # Caveats
///
/// `index`, at the top of the git directory, beside `HEAD` and `config`. A bare repository has no
/// index, and neither does a repository that has never had anything staged — which is why its
/// absence is an empty index rather than an error.
pub const INDEX_FILE: &str = "index";

/// Phase 3 — the four bytes an index file begins with.
pub const DIRC_MAGIC: &[u8; 4] = b"DIRC";

/// Phase 3 — the largest index version Rivet reads.
///
/// # Caveats
///
/// Three. v2 is what every Git writes by default and what Rivet writes; v3 exists only to carry the
/// extended flags word, and a repository with it is readable. **v4 is deliberately excluded**: it
/// prefix-compresses paths and removes entry padding, so a v2-shaped reader produces plausible
/// rubbish rather than an error. The plan puts v4 and the split index out of scope (§1), and
/// [`IndexVersion::from_u32`] refuses them so that the refusal happens at the header instead of
/// halfway through the entry list.
pub const MAX_INDEX_VERSION: u32 = 3;

/// Phase 3 — which DIRC revision an index file is.
///
/// # Caveats
///
/// * **Rivet writes [`IndexVersion::V2`] always**, even when it read a v3 file. v3's only addition
///   is the extended flags word, and writing v2 with those bits dropped would silently lose
///   `skip-worktree` — so a v3 index containing any extended entry must be written back as v3. The
///   writer's choice is therefore a function of its contents, not of a preference.
/// * **v4 is representable here only so it can be reported.** Constructing it is legal; parsing an
///   entry list as v4 is not implemented, and [`IndexVersion::from_u32`] is what a reader uses to
///   reject it before anything is misread.
/// * The version is big-endian on disk like every other integer in the file, but the field is
///   *not* a byte-order mark: each stat field is separately big-endian and they are not packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IndexVersion {
    /// Version 2 — the default since Git 1.6. Rivet's writer always emits this when it can.
    V2,
    /// Version 3 — v2 plus a second flags word carrying `skip-worktree` and extended mode bits.
    V3,
    /// Version 4 — prefix-compressed paths, no entry padding. Read by Git, not by Rivet.
    V4,
}

impl IndexVersion {
    /// Phase 3 — parse the version field of an index header.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnsupportedIndexVersion`] for anything other than 2, 3 or 4 — the
    /// value is reported in the error so a user can see what the file claims. Note that v4 parses
    /// here and is refused later, by whatever knows whether it can handle it.
    pub fn from_u32(version: u32) -> Result<Self> {
        match version {
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            4 => Ok(Self::V4),
            _ => Err(Error::UnsupportedIndexVersion { version }),
        }
    }

    /// Phase 3 — the number written into the header.
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::V2 => 2,
            Self::V3 => 3,
            Self::V4 => 4,
        }
    }

    /// Phase 3 — whether an entry under this version carries the second flags word.
    ///
    /// # Caveats
    ///
    /// True only for v3, and driven by the entry's `extended` bit rather than by the version alone:
    /// a v3 file may contain entries that do not use the extended form, and skipping the word for
    /// those is what keeps the entry list in step. The version answers "may an entry have one", the
    /// bit answers "does *this* entry".
    pub const fn has_extended_flags(self) -> bool {
        matches!(self, Self::V3)
    }
}

/// Phase 3 — which of the four slots a conflict occupies.
///
/// # Caveats
///
/// * **Stage 0 is the normal case and means "no conflict".** A path with a stage-0 entry is
///   resolved; a path with stages 1, 2 and 3 is unmerged, and Git never stores a stage 0 entry for
///   such a path at the same time.
/// * **The numbers are semantic, not ordinal.** Stage 1 is the *merge base*, stage 2 is *ours* and
///   stage 3 is *theirs*. Code that treats them as "first, second, third copy" gets the
///   three-way merge backwards, and the diff3 conflict markers depend on the distinction.
/// * **A path may have fewer than three non-zero stages.** A modify/delete conflict has no entry
///   for the deleted side (stages 1 and 2, or 1 and 3), and an add/add conflict has no stage 1.
///   "Unmerged" means "no stage 0", not "stages 1 through 3 all present".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// Stage 0 — a resolved entry. The overwhelming majority.
    Normal,
    /// Stage 1 — the merge base's version, present only during a conflict.
    Base,
    /// Stage 2 — the version from the branch being merged into ("ours").
    Ours,
    /// Stage 3 — the version from the branch being merged ("theirs").
    Theirs,
}

impl Stage {
    /// Phase 3 — decode the 2-bit stage field.
    ///
    /// # Caveats
    ///
    /// The field is two bits, so 0–3 are all the values that exist; a caller passing anything else
    /// has a bug rather than corrupt data, and this is one of the few places an argument's range
    /// genuinely is guaranteed by the format.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidStage`] for a value above 3. Reachable only from a caller that
    /// masked the wrong bits — which is a real mistake, since the stage lives in bits 12–13 and
    /// masking bits 13–14 instead silently produces stages 0 and 2 for everything.
    pub fn from_bits(bits: u8) -> Result<Self> {
        match bits {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Base),
            2 => Ok(Self::Ours),
            3 => Ok(Self::Theirs),
            _ => Err(Error::InvalidStage { stage: bits }),
        }
    }

    /// Phase 3 — the raw bits to write.
    pub const fn as_bits(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Base => 1,
            Self::Ours => 2,
            Self::Theirs => 3,
        }
    }

    /// Phase 3 — the stage as the small number Git prints, e.g. in `ls-files --stage`.
    ///
    /// # Caveats
    ///
    /// `ls-files --stage` prints `100644 <id> 0\tfile`, and the `0` there is this value — *not*
    /// [`Stage::as_bits`] with a different name. They happen to agree, and a future format change
    /// would separate them; keeping them distinct now means the printer does not have to change.
    pub const fn as_number(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Base => 1,
            Self::Ours => 2,
            Self::Theirs => 3,
        }
    }
}

/// Phase 3 — the two-byte flags word of an index entry.
///
/// # Caveats
///
/// * **[`IndexEntryFlags::assume_valid`] and `skip_worktree` are not the same thing**, though both
///   suppress work. `assume-valid` is a promise by the user that the file has not changed, and it
///   makes `status` skip it entirely — including its stat check. `skip-worktree` is a property of
///   a sparse checkout, and it makes the entry not be written to the work tree at all. Conflating
///   them makes `git update-index --assume-unchanged` corrupt a sparse checkout.
/// * **`assume_valid` lives in the first flags word and `skip_worktree` in the v3 extended word.**
///   A v2 index cannot record `skip-worktree` at all, which is why a repository that has any uses
///   v3.
/// * **`name_length` includes the terminating NUL**, so an entry reading `README\0` has 7 there.
///   Off-by-one here shifts every subsequent field, and the value saturates at
///   [`IndexEntryFlags::NAME_LENGTH_MAX`] for paths at or above 4095 bytes.
/// * **`extended` is a structural bit, not a property of the file.** It says the entry has a
///   second flags word on disk; a parser that ignores it walks into that word as if it were the
///   path, and produces entries whose paths are two bytes of flags followed by the real path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexEntryFlags(u16);

impl IndexEntryFlags {
    /// Phase 3 — the value at which the path length field saturates.
    ///
    /// # Caveats
    ///
    /// The field is 12 bits, so it holds 0–4095. A path whose stored length (including the NUL)
    /// reaches this value has it clamped, and the real length must be found by scanning for the NUL
    /// — a reader that trusts 4095 for such an entry reads 4095 bytes of a shorter path and then
    /// treats the next entry's header as part of it. Git's own constant, and the reason a path of
    /// exactly 4095 bytes and one of 4096 are indistinguishable from this field alone.
    pub const NAME_LENGTH_MAX: u16 = 0x0FFF;

    /// Phase 3 — a flags word with nothing set.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Phase 3 — read a flags word from its two on-disk bytes.
    ///
    /// # Caveats
    ///
    /// **Big-endian**, like every other multi-byte field in the file. Reading it little-endian
    /// produces a word whose stage bits are the low bits of the path length and whose path length is
    /// a few hundred — an entry that looks corrupt but parses.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Phase 3 — the word as two on-disk bytes.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Phase 3 — whether the entry has a second flags word (v3 only).
    pub const fn is_extended(self) -> bool {
        self.0 & 0x4000 != 0
    }

    /// Phase 3 — whether the user marked the entry assume-valid.
    ///
    /// # Caveats
    ///
    /// A promise from the user, not a fact about the file. Git trusts it completely: a file marked
    /// assume-valid that has changed is not noticed by `status`, is not updated by `add`, and is
    /// silently lost by a checkout that depends on the index being accurate. Rivet must honour it
    /// the same way, and must make `--no-assume-unchanged` the only way out.
    pub const fn assume_valid(self) -> bool {
        self.0 & 0x8000 != 0
    }

    /// Phase 3 — the stage bits, still encoded.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidStage`] for a value above 3. The field is two bits on disk so a
    /// value read from a file cannot trip this, but a hand-built word can, and a `Result` here is
    /// cheaper than a panic on repository-shaped data (plan §5).
    pub fn stage(self) -> Result<Stage> {
        Stage::from_bits(((self.0 >> 12) & 0x3) as u8)
    }

    /// Phase 3 — the stored path length, including the NUL.
    ///
    /// # Caveats
    ///
    /// Saturates; see [`IndexEntryFlags::NAME_LENGTH_MAX`]. The value is only usable as a length
    /// when it is below the saturation point, and a parser must scan for the NUL in either case
    /// rather than branching on it.
    pub const fn name_length(self) -> u16 {
        self.0 & 0x0FFF
    }

    /// Phase 3 — a flags word with the given stage and path length.
    ///
    /// # Caveats
    ///
    /// `path_len` is the path's byte length *excluding* the NUL, which is the value Git stores;
    /// the reader compares against the same count. Values at or above the 12-bit saturation point
    /// clamp to [`IndexEntryFlags::NAME_LENGTH_MAX`], which is why a writer must not rely on this
    /// field alone to recover a long path — the path itself is what the reader uses.
    pub const fn with(stage: Stage, path_len: usize) -> Self {
        let len = if path_len > 0x0FFF { 0x0FFF } else { path_len } as u16;
        Self((stage.as_bits() as u16) << 12 | len)
    }

    /// Phase 3 — replace the path-length field, keeping every other bit.
    ///
    /// The length is derived from the path and therefore has to be recomputed when an entry is
    /// serialised: an entry built in memory with [`IndexEntryFlags::empty`] carries a zero hint,
    /// and a reader that trusts it rejects the entry as corrupt.
    pub const fn with_name_length(self, path_len: usize) -> Self {
        let len = if path_len > 0x0FFF { 0x0FFF } else { path_len } as u16;
        Self((self.0 & !0x0FFF) | len)
    }

    /// Phase 3 — set the assume-valid bit.
    pub const fn set_assume_valid(self, on: bool) -> Self {
        Self(if on { self.0 | 0x8000 } else { self.0 & !0x8000 })
    }

    /// Phase 3 — set the extended bit, which forces the file to v3.
    ///
    /// # Caveats
    ///
    /// **The bit means "a second flags word follows", and setting it without writing one leaves the
    /// reader a field short for this entry and every one after it.** Exposed so that
    /// [`IndexEntryExtendedFlags::skip_worktree`] and the extended bit stay in step; a caller that
    /// sets the bit by hand must also write a second flags word for that entry.
    pub const fn set_extended(self, on: bool) -> Self {
        Self(if on { self.0 | 0x4000 } else { self.0 & !0x4000 })
    }
}

/// Phase 3 — the second flags word, present only for an extended (v3) entry.
///
/// # Caveats
///
/// * **Only bit 14 has a meaning Rivet implements**: `skip-worktree`. Bits 0–12 are the *extended
///   mode bits*, used by a repository that stores modes beyond the five canonical ones — a
///   `core.filemode` experiment, or a Windows checkout that needs a mode Git does not model. Rivet
///   preserves the word verbatim and does not interpret those bits, which is the only safe option:
///   guessing at a mode change would make `status` report differences that are not there.
/// * **Bits 13 and 14 are reserved and must be written as zero**, matching Git. A file with them set
///   is not one Git writes, and a reader that echoed them back would propagate a corruption.
/// * **A word that is present but all zero is normal.** An entry may be marked extended and use none
///   of it; the bit and the word's contents are independent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexEntryExtendedFlags(u16);

impl IndexEntryExtendedFlags {
    /// Phase 3 — a word with nothing set.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Phase 3 — read the word from its two on-disk bytes.
    ///
    /// # Caveats
    ///
    /// Big-endian, like the first flags word and every other integer in the file. The two words are
    /// adjacent and both are read the same way; a reader that treats only the first as big-endian
    /// swaps `skip-worktree` with a mode bit.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Phase 3 — the word as two on-disk bytes.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Phase 3 — whether the entry is a sparse-checkout placeholder.
    ///
    /// # Caveats
    ///
    /// A skipped entry is **not written to the work tree** and **not reported as deleted** by
    /// `status`, even though its file is absent. A checkout that ignores this writes every sparse
    /// file back and silently undoes the sparse-checkout; a `status` that ignores it reports the
    /// whole sparse cone as deleted.
    pub const fn skip_worktree(self) -> bool {
        self.0 & 0x4000 != 0
    }

    /// Phase 3 — set the skip-worktree bit.
    pub const fn set_skip_worktree(self, on: bool) -> Self {
        Self(if on { self.0 | 0x4000 } else { self.0 & !0x4000 })
    }

    /// Phase 3 — the extended mode bits, uninterpreted.
    ///
    /// # Caveats
    ///
    /// **Not a mode.** The low 13 bits of this word are the *high* part of a mode Git does not
    /// otherwise store; the real mode is [`IndexEntry::mode`] combined with these, and a caller that
    /// treats this value as a mode on its own gets a number below `0o2000` for every entry. Rivet
    /// keeps it so a round trip is lossless and deliberately does not use it.
    pub const fn extended_mode_bits(self) -> u16 {
        self.0 & 0x1FFF
    }
}

/// Phase 3 — one entry of the index.
///
/// The stat fields are a cache of the work tree file's `lstat`, the object id is the content that
/// was staged, and the flags carry the stage and the path's length.
///
/// # Caveats
///
/// * **`dev`, `ino`, `uid` and `gid` are truncated to 32 bits.** The on-disk format predates 64-bit
///   inodes and, on a filesystem with large ones, the value stored is the low half. Comparing a
///   `stat` result against them therefore requires masking to 32 bits first; comparing the full
///   64-bit value always mismatches and makes every file look modified.
/// * **`ctime` is the inode-change time, not the creation time.** Unix has no creation time in
///   `stat`'s classic form, and Git uses `ctime` as a third signal that the file changed — a
///   `chmod` updates it, which is deliberate.
/// * **The nanoseconds fields are a real part of the comparison.** A filesystem with sub-second
///   timestamps (everything modern on Linux) can write the same file twice within a second, and
///   comparing only the seconds field lets a modification go unnoticed. On a filesystem that
///   stores no nanoseconds, they are zero and the racy-git rule is what protects correctness.
/// * **`mode` has only three legal values**, and it is an [`EntryMode`] here rather than a `u32` so
///   that the normalisation can happen once at parse. `100664` and `100644` are the same entry as
///   far as Git is concerned; the file-type bits (`100000`, `120000`, `160000`) are the part that
///   matters.
/// * **`path` is bytes.** A repository can contain a filename that is not valid UTF-8 on Linux, and
///   the index stores it exactly. Comparing it with a `String` requires a lossy conversion that
///   would make two different paths equal.
/// * **A path is relative to the work tree root and uses `/` on every platform.** It never starts
///   with a slash, never contains `..` and never contains a NUL — a NUL terminates it on disk, so a
///   path cannot contain one — and Git's `fsck` rejects an index that violates those rules because
///   a checkout would otherwise escape the work tree.
/// * **The struct owns its path.** The index is a transaction type that outlives the buffer it was
///   parsed from (see `object/mod.rs`'s rule), so the path cannot borrow.
/// * **`Ord` is *not* the on-disk order.** The derived ordering compares the stat fields first,
///   which is meaningless for a sort. The index's ordering — path bytes, then stage — is
///   [`compare_entries`], because getting it wrong makes the file unsorted and Git unable to
///   binary-search it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Inode-change time, seconds. A third change signal on top of mtime and size.
    pub ctime_seconds: u32,
    /// Inode-change time, nanoseconds.
    pub ctime_nanos: u32,
    /// Modification time, seconds. The primary stat signal.
    pub mtime_seconds: u32,
    /// Modification time, nanoseconds.
    pub mtime_nanos: u32,
    /// Device number, truncated to 32 bits. Compare a `stat` result masked, not raw.
    pub dev: u32,
    /// Inode number, truncated to 32 bits. Same caveat as [`IndexEntry::dev`].
    pub ino: u32,
    /// The file mode, already normalised to one of Git's five legal values.
    pub mode: EntryMode,
    /// Owner uid, truncated to 32 bits.
    pub uid: u32,
    /// Owner gid, truncated to 32 bits.
    pub gid: u32,
    /// File size, truncated to 32 bits. A 5 GB file has a size here that is its true size mod 2^32.
    pub size: u32,
    /// The id of the blob holding this path's staged content.
    ///
    /// # Caveats
    ///
    /// Not necessarily present in the object database: an id can name an object that a `gc` has
    /// removed, and a `-N` entry names the empty blob without reading it. Nothing here verifies it.
    pub oid: ObjectId,
    /// The flags word: stage, assume-valid, extended, and the stored path length.
    pub flags: IndexEntryFlags,
    /// The second flags word, meaningful only when [`IndexEntryFlags::is_extended`] is set.
    ///
    /// # Caveats
    ///
    /// **Written verbatim and not interpreted here.** In v3 it carries `skip-worktree` in bit 15 and
    /// extended mode bits in 0–12, and a repository that uses extended mode bits has entries whose
    /// real mode is not [`IndexEntry::mode`]. Rivet stores the word so it round-trips; a caller that
    /// needs a mode beyond the five canonical ones is out of scope for v1 and must not silently use
    /// the truncated one.
    pub extended_flags: u16,
    /// The path, relative to the work tree root, `/`-separated, as raw bytes.
    pub path: BString,
}

impl IndexEntry {
    /// Phase 3 — parse one entry from the start of `data`.
    ///
    /// Returns the entry and how many bytes it occupied, padding included — the only way to find
    /// the next entry, since the padding is not recorded anywhere.
    ///
    /// # Caveats
    ///
    /// * `format` decides the id width, which is not in the file; get it wrong and the path bytes
    ///   are read as an id.
    /// * `version` decides whether the extended flags word may be present, and the entry's own
    ///   `extended` bit decides whether it is. Both are needed, and neither alone is enough.
    /// * The consumed length is a multiple of eight, measured **from the entry's start**. A parser
    ///   that pads to align the file offset works right up until the first entry whose length is not
    ///   already a multiple of eight — which for a repository with a 5-character filename is the
    ///   first entry of the first index anyone parses.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptIndex`] for a truncated entry, a path with no NUL before the
    /// end, a stage above 3, a mode that is not one of the five (reported from
    /// [`crate::error::Error::InvalidEntryMode`]), and [`crate::error::Error::InvalidStage`] for
    /// malformed stage bits; [`crate::error::Error::IndexEntryTooLong`] for a path longer than the
    /// index allows.
    pub fn parse(
        format: ObjectFormat,
        version: IndexVersion,
        data: &[u8],
    ) -> Result<(Self, usize)> {
        const FIXED: usize = 10 * 4;
        let id_len = format.len();
        if data.len() < FIXED {
            return Err(Error::CorruptIndex {
                offset: data.len(),
                reason: "entry shorter than the fixed part".into(),
            });
        }
        let take = |i: usize| u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        let ctime_seconds = take(0);
        let ctime_nanos = take(4);
        let mtime_seconds = take(8);
        let mtime_nanos = take(12);
        let dev = take(16);
        let ino = take(20);
        let mode = take(24);
        let uid = take(28);
        let gid = take(32);
        let size = take(36);

        let id_end = FIXED + id_len;
        if data.len() < id_end {
            return Err(Error::CorruptIndex {
                offset: id_end,
                reason: "truncated object id".into(),
            });
        }
        let oid = ObjectId::from_bytes(format, &data[FIXED..id_end])?;

        let flags = IndexEntryFlags::from_bits(
            u16::from_be_bytes([data[id_end], data[id_end + 1]]),
        );
        if data.len() < id_end + 2 {
            return Err(Error::CorruptIndex {
                offset: id_end + 2,
                reason: "truncated flags word".into(),
            });
        }
        let extended_flags = if flags.is_extended() {
            if !version.has_extended_flags() {
                return Err(Error::CorruptIndex {
                    offset: id_end + 2,
                    reason: "extended flags word in a version that does not carry one".into(),
                });
            }
            if data.len() < id_end + 4 {
                return Err(Error::CorruptIndex {
                    offset: id_end + 4,
                    reason: "truncated extended flags word".into(),
                });
            }
            u16::from_be_bytes([data[id_end + 2], data[id_end + 3]])
        } else {
            0
        };

        let stage = flags.stage()?;

        // The stored name length is a hint only: it is the byte count before the NUL, saturated at
        // 4095. Locate the NUL by scanning; the hint must match when it is below the saturation
        // point.
        let path_start = id_end + if flags.is_extended() { 4 } else { 2 };
        let hint = flags.name_length();
        let nul = data[path_start..].iter().position(|&b| b == 0);
        let Some(rel) = nul else {
            return Err(Error::CorruptIndex {
                offset: data.len(),
                reason: "path not NUL-terminated".into(),
            });
        };
        let len = rel as u16;
        if hint < IndexEntryFlags::NAME_LENGTH_MAX && hint != len {
            return Err(Error::CorruptIndex {
                offset: path_start + len as usize,
                reason: "stored name length disagrees with the path".into(),
            });
        }
        if rel > INDEX_PATH_MAX {
            return Err(Error::IndexEntryTooLong {
                path: BString::from(data[path_start..path_start + rel].to_vec()),
            });
        }
        let path = BString::from(data[path_start..path_start + rel].to_vec());

        let mode = match entry_mode_from_u32(mode) {
            Some(m) => m,
            None => {
                return Err(Error::InvalidEntryMode {
                    name: path.clone(),
                    mode,
                })
            }
        };

        let fixed = FIXED + id_len + 2 + if flags.is_extended() { 2 } else { 0 };
        let total = fixed + len as usize + 1;
        let consumed = (total + 7) / 8 * 8;
        if data.len() < consumed {
            return Err(Error::CorruptIndex {
                offset: consumed,
                reason: "entry runs past the end of the data".into(),
            });
        }

        Ok((
            Self {
                ctime_seconds,
                ctime_nanos,
                mtime_seconds,
                mtime_nanos,
                dev,
                ino,
                mode,
                uid,
                gid,
                size,
                oid,
                flags,
                extended_flags,
                path,
            },
            consumed,
        ))
    }

    /// Phase 3 — serialise the entry, padding included.
    ///
    /// # Caveats
    ///
    /// * The padding is computed from the entry's own length, so that `parse` and `to_bytes`
    ///   round-trip exactly. Producing a different amount of padding than Git for the same entry is
    ///   not a correctness problem — Git reads it — but it makes every differential comparison of
    ///   the file bytes fail, and it is a signal that the alignment rule is understood wrong.
    /// * A path containing a NUL is impossible to serialise, because the NUL is the terminator. The
    ///   check belongs here rather than at every call site, because this is the last moment it can
    ///   be caught before the file is unreadable.
    /// * The `extended` bit and [`IndexEntry::extended_flags`] must agree: a word written without
    ///   the bit set is read as the path's first two bytes, and a bit set with no word leaves the
    ///   reader one field short for every subsequent entry.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptIndex`] for a path containing a NUL, and
    /// [`crate::error::Error::IndexEntryTooLong`] for a path past the format's limit.
    pub fn to_bytes(&self, format: ObjectFormat, version: IndexVersion) -> Result<Vec<u8>> {
        if self.path.contains(&0) {
            return Err(Error::CorruptIndex {
                offset: 0,
                reason: "path contains a NUL".into(),
            });
        }
        if self.path.len() > INDEX_PATH_MAX {
            return Err(Error::IndexEntryTooLong {
                path: self.path.clone(),
            });
        }
        let ext_word = self.flags.is_extended();
        if ext_word && !version.has_extended_flags() {
            return Err(Error::CorruptIndex {
                offset: 0,
                reason: "extended flags word in a version that does not carry one".into(),
            });
        }
        let mut out = Vec::with_capacity(
            40 + format.len() + 2 + usize::from(ext_word) * 2 + self.path.len() + 1 + 8,
        );
        out.extend_from_slice(&self.ctime_seconds.to_be_bytes());
        out.extend_from_slice(&self.ctime_nanos.to_be_bytes());
        out.extend_from_slice(&self.mtime_seconds.to_be_bytes());
        out.extend_from_slice(&self.mtime_nanos.to_be_bytes());
        out.extend_from_slice(&self.dev.to_be_bytes());
        out.extend_from_slice(&self.ino.to_be_bytes());
        out.extend_from_slice(&self.mode.to_u32().to_be_bytes());
        out.extend_from_slice(&self.uid.to_be_bytes());
        out.extend_from_slice(&self.gid.to_be_bytes());
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(self.oid.as_bytes());
        // The 12-bit path-length field is derived from the path, so recompute it here rather than
        // trusting whatever the in-memory entry was built with: an entry created through
        // `IndexEntryFlags::empty()` carries a zero hint, and the parser rejects a mismatch.
        let flags = self.flags.with_name_length(self.path.len());
        out.extend_from_slice(&flags.bits().to_be_bytes());
        if ext_word {
            out.extend_from_slice(&self.extended_flags.to_be_bytes());
        }
        out.extend_from_slice(&self.path);
        out.push(0);
        let target = (out.len() + 7) / 8 * 8;
        out.resize(target, 0);
        Ok(out)
    }

    /// Phase 3 — the stage this entry is in.
    pub fn stage(&self) -> Stage {
        self.flags.stage().unwrap_or(Stage::Normal)
    }

    /// Phase 3 — whether the path has conflict stages rather than a single resolved entry.
    ///
    /// # Caveats
    ///
    /// True for every non-zero stage, which means a path with stages 1, 2 and 3 reports `true` three
    /// times. The useful question is usually per-path — [`Index::has_conflicts`] for the whole index
    /// and [`IndexExt::is_unmerged`] for one path — and this only answers for the entry in hand.
    pub fn is_unmerged(&self) -> bool {
        self.stage() != Stage::Normal
    }

    /// Phase 3 — whether this entry is a directory placeholder rather than a file.
    ///
    /// # Caveats
    ///
    /// False in practice: Git stores no tree entries in the index at all, and an entry with
    /// [`EntryMode::Tree`] comes from a hand-edited or foreign index. Rivet must still accept one
    /// rather than fail, because refusing means being unable to read a file Git reads — but it must
    /// not *write* one, and a `commit` built from such an index has to decide what it means.
    pub fn is_tree(&self) -> bool {
        matches!(self.mode, EntryMode::Tree)
    }

    /// Phase 3 — whether the entry's mode says it is a symlink.
    ///
    /// # Caveats
    ///
    /// The blob's content is the link target, so comparing such an entry against the work tree means
    /// reading the link (`readlink`) rather than the file — and the size and mtime in the entry are
    /// those of the link itself, not of anything it points at. A `stat` where an `lstat` is wanted
    /// follows the link and compares the wrong file's size, which makes a symlink appear modified
    /// whenever its target changes length.
    pub fn is_symlink(&self) -> bool {
        matches!(self.mode, EntryMode::Link)
    }

    /// Phase 3 — whether the entry is a submodule.
    ///
    /// # Caveats
    ///
    /// Such an entry's object id is a **commit id in another repository**, not a blob. Rivet does
    /// not implement submodules (plan §1) but must preserve the entry across `add`, `commit` and
    /// checkout without ever reading its id out of this repository's object database — where it
    /// will not be found, and where a "not found" that aborts a status is the difference between
    /// preserving a submodule and refusing to work in a repository that has one.
    pub fn is_gitlink(&self) -> bool {
        matches!(self.mode, EntryMode::Commit)
    }

    /// Phase 3 — whether the entry's content is the empty blob.
    ///
    /// # Caveats
    ///
    /// Computed by hashing an empty body, which is cheaper than reading any object but still a hash
    /// per call in a loop. Notably, an `intent-to-add` entry has this id, so this is not a test for
    /// "the file is empty" — the file may have content that has never been staged.
    pub fn is_empty_blob(&self, format: ObjectFormat) -> bool {
        hash_object(format, ObjectKind::Blob, &[]).is_ok_and(|(empty, _)| self.oid == empty)
    }
}

/// Phase 3 — the order index entries are stored in.
///
/// # Caveats
///
/// * **Path bytes first, compared with `memcmp`**, so `a.b` sorts before `a/b` — `.` is 0x2E and
///   `/` is 0x2F. That is the opposite of the intuitive "files before subdirectories", and the
///   reason a tree's sorting rule (directories compared as if a `/` were appended) is *not* this
///   one. Using the tree comparator here produces an index that looks sorted to a human and that
///   Git's binary search cannot find anything in.
/// * **Ties are broken by stage ascending**, which is the only way two entries can share a path:
///   1, 2, 3 for a conflict. Equal paths with equal stages is a duplicate entry, and
///   [`Index::sort`] must not reorder them silently — `fsck` reports it.
/// * This is a free function rather than an `Ord` impl on [`IndexEntry`], because the derived `Ord`
///   already means something else (a field-wise comparison, used nowhere) and overloading it would
///   make `entries.sort()` in a caller look correct while producing a file Git rejects.
pub fn compare_entries(a: &IndexEntry, b: &IndexEntry) -> Ordering {
    match a.path.as_slice().cmp(b.path.as_slice()) {
        Ordering::Equal => a.stage().as_bits().cmp(&b.stage().as_bits()),
        ord => ord,
    }
}

/// Phase 3 — an index extension, kept verbatim.
///
/// # Caveats
///
/// * **The signature's first byte's case is a capability flag, not decoration.** A lowercase first
///   letter marks the extension *optional*: a reader that does not understand it may skip it, and a
///   writer that would drop it is allowed to. An uppercase one marks it *mandatory*: a reader that
///   does not understand it must refuse the whole index, because ignoring a mandatory extension
///   changes what the index means. `TREE`, `REUC` and `UNTR` are all optional.
/// * **The length is the payload's, and the payload is not padded.** Extensions follow one another
///   with no alignment, which is why unknown ones can be skipped exactly rather than heuristically.
/// * **The signature is four arbitrary bytes**, not necessarily ASCII. It is compared byte-exactly.
/// * `TREE` in particular must round-trip even though Rivet never reads it: it caches the tree a
///   commit will write, and dropping it makes the next `git commit` rebuild a tree it did not need
///   to — and, worse, makes the index Rivet writes differ from the one Git would have written for
///   the same content, for no semantic gain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexExtension {
    /// The four-byte signature, e.g. `TREE` or `link`.
    pub signature: [u8; 4],
    /// The payload, exactly as it appeared.
    pub data: Vec<u8>,
}

impl IndexExtension {
    /// Phase 3 — build an extension from a signature and a payload.
    pub fn new(signature: [u8; 4], data: Vec<u8>) -> Self {
        Self { signature, data }
    }

    /// Phase 3 — whether the extension is optional, from the case of its first signature byte.
    ///
    /// # Caveats
    ///
    /// A signature whose first byte is not an ASCII letter is *not* optional by this rule, which
    /// makes an unknown one fatal. That is the conservative direction and it matches Git: the
    /// implementation checks `islower()` on the byte, and a byte that is not a cased letter is not
    /// lower.
    pub fn is_optional(&self) -> bool {
        self.signature[0].is_ascii_lowercase()
    }

    /// Phase 3 — the signature as text, for diagnostics.
    ///
    /// # Caveats
    ///
    /// Lossy. A signature is four arbitrary bytes and a hostile index can put anything there; this
    /// is for an error message, never for a comparison.
    pub fn signature_str(&self) -> String {
        String::from_utf8_lossy(&self.signature).into_owned()
    }

    /// Phase 3 — whether this is the cached-tree extension.
    pub fn is_tree(&self) -> bool {
        self.signature == *b"TREE"
    }

    /// Phase 3 — whether this is the resolve-undo extension.
    pub fn is_reuc(&self) -> bool {
        self.signature == *b"REUC"
    }

    /// Phase 3 — whether this is the untracked-cache extension.
    pub fn is_untracked_cache(&self) -> bool {
        self.signature == *b"UNTR"
    }

    /// Phase 3 — whether this is the split-index link extension.
    ///
    /// # Caveats
    ///
    /// A `link` extension means the file is a **split index**: the real entries live in a shared
    /// `sharedindex.<hex>` file and this one holds only the deltas. Split index is out of scope for
    /// v1 (plan §1), so this being present is a reason to refuse the file rather than to read
    /// nothing and report an empty index — which is what a reader that ignored the extension would
    /// do, and which makes every file look deleted.
    pub fn is_split_index_link(&self) -> bool {
        self.signature == *b"link"
    }
}

/// Phase 3 — a parsed `.git/index`.
///
/// # Caveats
///
/// * **An index may legitimately be empty**, and its absence on disk is the same thing: a fresh
///   repository has no `index` file at all until something is staged, and a `git rm --cached` of
///   everything leaves one with zero entries. Both mean "nothing staged", and neither is an error.
/// * **The entries vector is authoritative and must be kept sorted.** Every mutating method here is
///   documented with whether it preserves the order; a caller that pushes directly must sort before
///   writing, and [`Index::sort`] is not automatic because a sort in the middle of a merge's
///   stage-rewriting would be a silent correctness fix that hides a bug.
/// * **The checksum is not stored in the struct and is recomputed on write.** Keeping it would mean
///   a struct that is valid only when its bytes have not changed, which is a state no field can
///   enforce; [`Index::parse`] verifies it and then drops it, and [`Index::to_bytes`] appends a
///   fresh one.
/// * **The file's own mtime is not tracked here.** The racy-git rule needs it — see
///   [`Index::is_racy`] — and it comes from the filesystem at the call site, not from the file's
///   contents, because passing it in is the only way to make the comparison testable.
/// * **The version is preserved on read but not necessarily on write.** A v3 index with no extended
///   entries is written back as v2, which is what Git does and what keeps a repository from
///   accumulating needless version bumps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// The version this index was read as, or the one it should be written as.
    version: IndexVersion,
    /// The entries, in the order they were read. Sorted, if the file was well-formed.
    entries: Vec<IndexEntry>,
    /// Extensions, in file order, kept verbatim.
    extensions: Vec<IndexExtension>,
}

impl Index {
    /// Phase 3 — a new, empty index at the version Rivet writes.
    ///
    /// # Caveats
    ///
    /// `git init` does not create the index file; it appears on the first `add`. An empty `Index`
    /// written out is a valid file with a zero entry count, and Git accepts it — but writing one
    /// where Git would have written nothing changes the file's mtime, which matters to the racy
    /// rule and to any differential comparison of the repository's files.
    pub fn new() -> Self {
        Self {
            version: IndexVersion::V2,
            entries: Vec::new(),
            extensions: Vec::new(),
        }
    }

    /// Phase 3 — read and verify `path`.
    ///
    /// # Caveats
    ///
    /// **A missing file is an empty index, not an error.** Nearly every repository starts that way,
    /// and a caller that had to distinguish the two would have to stat the path itself — which is
    /// exactly the check this does, so it may as well return the answer.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the file exists but cannot be read;
    /// [`crate::error::Error::BadIndexSignature`] for a file that does not start with `DIRC`;
    /// [`crate::error::Error::UnsupportedIndexVersion`] for v4 or an unknown version;
    /// [`crate::error::Error::BadIndexChecksum`] if the trailer does not match;
    /// [`crate::error::Error::CorruptIndex`] for a truncated or malformed body.
    pub fn load(path: &Path, format: ObjectFormat) -> Result<Self> {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::new())
            }
            Err(e) => {
                return Err(Error::IoAtPath {
                    path: BString::from(path.to_string_lossy().as_bytes()),
                    source: e,
                })
            }
        };
        if data.len() < 12 {
            return Err(Error::CorruptIndex {
                offset: 0,
                reason: "truncated",
            });
        }
        if &data[0..4] != b"DIRC" {
            return Err(Error::BadIndexSignature {
                found: [data[0], data[1], data[2], data[3]],
            });
        }
        let version_u32 = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let version = IndexVersion::from_u32(version_u32)?;
        if version == IndexVersion::V4 {
            return Err(Error::UnsupportedIndexVersion {
                version: version_u32,
            });
        }
        let entry_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        // The trailer checksum is not extension data: every scan for entries and extensions must
        // stop at `body_end`, or the parser reads the checksum's first bytes as an extension header
        // and calls a well-formed file truncated.
        let trailer = format.len();
        if data.len() < 12 + trailer {
            return Err(Error::CorruptIndex {
                offset: data.len(),
                reason: "missing checksum",
            });
        }
        let body_end = data.len() - trailer;
        let mut offset = 12usize;
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let (entry, consumed) = IndexEntry::parse(format, version, &data[offset..body_end])?;
            offset += consumed;
            entries.push(entry);
        }
        let mut extensions = Vec::new();
        while offset < body_end {
            if body_end - offset < 8 {
                return Err(Error::CorruptIndex {
                    offset,
                    reason: "truncated extension",
                });
            }
            let sig = [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]];
            let ext_len = u32::from_be_bytes([data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7]]) as usize;
            offset += 8;
            if body_end - offset < ext_len {
                return Err(Error::CorruptIndex {
                    offset,
                    reason: "truncated extension data",
                });
            }
            let ext_data = data[offset..offset + ext_len].to_vec();
            offset += ext_len;
            extensions.push(IndexExtension::new(sig, ext_data));
        }
        let expected_checksum = ObjectId::read(format, &data[body_end..])?;
        let mut hasher = format.hasher();
        hasher.update(&data[..body_end]);
        let actual_checksum = hasher.finish().map_err(|_| Error::CorruptIndex {
            offset: 0,
            reason: "checksum computation failed".into(),
        })?;
        if expected_checksum != actual_checksum {
            return Err(Error::BadIndexChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            });
        }
        Ok(Self {
            version,
            entries,
            extensions,
        })
    }

    /// Phase 3 — parse and verify index bytes.
    ///
    /// The whole verification sequence, in order: check the magic, read the version, read the entry
    /// count, parse that many entries tracking the offset, parse extensions until the checksum,
    /// then recompute the checksum over everything before it and compare. Skipping the last step is
    /// the tempting one — the parse already succeeded, so the file is *well-formed* — and it is
    /// wrong: a well-formed garbage file is exactly what a concurrent writer's partial state looks
    /// like, and the checksum is the only thing that tells the two apart.
    ///
    /// # Errors
    ///
    /// As [`Index::load`].
    pub fn parse(format: ObjectFormat, data: &[u8]) -> Result<Self> {
        let data_len = data.len();
        if data_len < 12 {
            return Err(Error::CorruptIndex { offset: 0, reason: "truncated index" });
        }
        if &data[0..4] != b"DIRC" {
            return Err(Error::BadIndexSignature { found: [data[0], data[1], data[2], data[3]] });
        }
        let version_u32 = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let version = IndexVersion::from_u32(version_u32)?;
        let entry_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let mut offset: usize = 12;
        let mut entries = Vec::with_capacity(entry_count as usize);
        for _ in 0..entry_count {
            let (entry, consumed) = IndexEntry::parse(format, version, &data[offset..])?;
            offset += consumed;
            entries.push(entry);
        }
        let mut extensions = Vec::new();
        while offset < data_len {
            if data_len - offset < 8 {
                return Err(Error::CorruptIndex { offset, reason: "truncated extension" });
            }
            let sig = [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]];
            let ext_len = u32::from_be_bytes([data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7]]) as usize;
            offset += 8;
            if offset + ext_len > data_len {
                return Err(Error::CorruptIndex { offset, reason: "extension extends past end" });
            }
            let ext_data = data[offset..offset + ext_len].to_vec();
            extensions.push(IndexExtension::new(sig, ext_data));
            offset += ext_len;
        }
        if offset + 20 > data_len {
            return Err(Error::CorruptIndex { offset, reason: "missing checksum" });
        }
        let expected = ObjectId::read(format, &data[offset..offset + 20])?;
        let mut hasher = format.hasher();
        hasher.update(&data[..offset]);
        let actual = hasher.finish()?;
        if expected != actual {
            return Err(Error::BadIndexChecksum { expected, actual });
        }
        Ok(Self { version, entries, extensions })
    }

    /// Phase 3 — serialise with a fresh trailing checksum.
    ///
    /// # Caveats
    ///
    /// * The version written is [`IndexVersion::V2`] unless some entry needs the extended word, in
    ///   which case it is v3. Writing v2 for an index that has a `skip-worktree` entry silently
    ///   drops the flag — see [`IndexVersion`].
    /// * Extensions are written **after the entries and before the checksum**, in the order they
    ///   were read, so a `TREE` extension Git wrote survives a Rivet `add`. Dropping them would be
    ///   simpler and would make Rivet's index observably worse than Git's.
    /// * The returned bytes include the checksum. There is no form that returns the body alone,
    ///   because a caller that forgot to append one would write an index that every reader rejects.
    /// * An index whose entries are out of order is written out of order. Sorting here would hide
    ///   the caller's bug and, worse, would silently change which of two conflicting entries is
    ///   found first — [`Index::sort`] is the explicit operation.
    pub fn to_bytes(&self, format: ObjectFormat) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        let version = if self.entries.iter().any(|e| e.flags.is_extended()) {
            IndexVersion::V3
        } else {
            IndexVersion::V2
        };
        // Git's index header is big-endian, like every other on-disk integer Git writes.
        out.extend_from_slice(&version.as_u32().to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for entry in &self.entries {
            let bytes = entry.to_bytes(format, version).expect("entry serialization failed");
            out.extend_from_slice(&bytes);
        }
        for ext in &self.extensions {
            out.extend_from_slice(&ext.signature);
            // A cached extension is a signature, a big-endian byte count, then the bytes; writing
            // the payload without its length produces a file whose next read walks into the data.
            out.extend_from_slice(&(ext.data.len() as u32).to_be_bytes());
            out.extend_from_slice(&ext.data);
        }
        let mut hasher = format.hasher();
        hasher.update(&out);
        let checksum = hasher.finish().expect("hash computation failed");
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    /// Phase 3 — write to `path` atomically, via `path.lock`.
    ///
    /// # Caveats
    ///
    /// * **The index has its own lock, `index.lock`**, and every Git process respects it. An index
    ///   written without the lock can be interleaved with a concurrent `git add`, which produces a
    ///   file whose checksum is valid for a mixture of two states — the corruption the checksum is
    ///   least able to help with.
    /// * **Written 0644, not read-only.** Unlike a loose object, an index is rewritten constantly;
    ///   the lock and the rename are what make that safe.
    /// * The rename is the commit point, so a caller that has also touched the work tree — a
    ///   checkout — must order the two so that a crash leaves the work tree and the index agreeing
    ///   or the operation re-runnable.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] if `index.lock` exists, and
    /// [`crate::error::Error::IoAtPath`] if the write or rename fails.
    pub fn write(&self, path: &Path, format: ObjectFormat) -> Result<()> {
        let lock_path = path.with_file_name("index.lock");
        if lock_path.exists() {
            return Err(Error::LockHeld { path: BString::from(path.to_string_lossy().as_bytes()) });
        }
        let bytes = self.to_bytes(format);
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("index");
        let tmp_path = path.with_file_name(format!("{}.tmp", file_name));
        let result = std::fs::write(&tmp_path, &bytes);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(Error::IoAtPath { path: BString::from(path.to_string_lossy().as_bytes()), source: e });
        }
        let result = std::fs::rename(&tmp_path, path);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(Error::IoAtPath { path: BString::from(path.to_string_lossy().as_bytes()), source: e });
        }
        let _ = std::fs::remove_file(&lock_path);
        Ok(())
    }

    /// Phase 3 — the index version this was read as.
    pub fn version(&self) -> IndexVersion {
        self.version
    }

    /// Phase 3 — the entries, in file order.
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// Phase 3 — the entries, mutably.
    ///
    /// # Caveats
    ///
    /// **The order is the caller's problem.** Pushing, removing or replacing through this can leave
    /// the index unsorted, and an unsorted index is one Git cannot binary-search — `git add` will
    /// fail to find an entry that is visibly present. Call [`Index::sort`] before writing unless the
    /// change provably preserves order.
    pub fn entries_mut(&mut self) -> &mut Vec<IndexEntry> {
        &mut self.entries
    }

    /// Phase 3 — the extensions, in file order.
    pub fn extensions(&self) -> &[IndexExtension] {
        &self.extensions
    }

    /// Phase 3 — the extensions, mutably.
    pub fn extensions_mut(&mut self) -> &mut Vec<IndexExtension> {
        &mut self.extensions
    }

    /// Phase 3 — how many entries there are.
    ///
    /// # Caveats
    ///
    /// **Not the number of paths.** An unmerged path contributes one entry per stage, so an index
    /// with a three-way conflict has three more entries than paths. `git ls-files | wc -l` counts
    /// entries and `git status` counts paths; the two disagree exactly during a merge, which is when
    /// someone is looking.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Phase 3 — whether nothing is staged.
    ///
    /// # Caveats
    ///
    /// An index with only unmerged entries is not empty, and a path with stages 1–3 makes this
    /// `false` while there is no stage-0 entry at all — so this is not "everything is staged
    /// cleanly", which is [`Index::has_conflicts`] asked the other way.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Phase 3 — find the stage-0 entry for a path.
    ///
    /// # Caveats
    ///
    /// * **Binary search on the sorted order**, which is why an unsorted index gives a wrong answer
    ///   rather than an error. When the index is known to be freshly parsed this is exact; when a
    ///   caller has been mutating, it may be searching a file that no longer matches.
    /// * **A conflicted path has no stage-0 entry**, so `None` here is ambiguous between "not
    ///   tracked" and "unmerged". Callers in a merge must check [`Index::has_conflicts`] or use
    ///   [`IndexExt::is_unmerged`], because treating a conflict as untracked makes `status` report a
    ///   file that is very much tracked as new.
    pub fn entry(&self, path: &BStr) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| &e.path == path && e.stage() == Stage::Normal)
    }

    /// Phase 3 — find a specific stage of a path.
    ///
    /// # Caveats
    ///
    /// A missing entry is `Ok(None)`, never [`crate::error::Error::PathNotFound`] — a lookup that
    /// failed because nothing matched is an answer, and a caller doing a three-way merge asks for
    /// stages that legitimately do not exist (a modify/delete conflict has no stage 3). The
    /// `Result` is for the sort-order precondition, not for the lookup.
    ///
    /// # Errors
    ///
    /// Nothing in normal use. The signature is fallible so that an implementation that detects an
    /// out-of-order entry list — where a binary search would silently miss a present entry — can
    /// report it instead of returning `None`.
    pub fn entry_at(&self, path: &BStr, stage: Stage) -> Result<Option<&IndexEntry>> {
        if !self.is_sorted() {
            return Err(Error::CorruptIndex {
                offset: 0,
                reason: "index entries are not in Git sort order",
            });
        }
        let bits = stage.as_bits();
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = &self.entries[mid];
            let ord = match e.path.as_slice().cmp(&path[..]) {
                Ordering::Equal => e.stage().as_bits().cmp(&bits),
                ord => ord,
            };
            match ord {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(Some(e)),
            }
        }
        Ok(None)
    }

    /// Phase 3 — insert or replace an entry.
    ///
    /// # Caveats
    ///
    /// * **Replaces the entry with the same path *and stage***, not the same path — inserting a
    ///   stage-2 entry beside an existing stage-1 one is how a merge conflict is built, and replacing
    ///   by path alone would destroy the base version.
    /// * Returns the entry it displaced, so a caller can report a real change as one.
    /// * **Does not maintain sort order.** The entry is appended when nothing matched; a caller
    ///   inserting several must call [`Index::sort`] before writing.
    pub fn insert(&mut self, entry: IndexEntry) -> Option<IndexEntry> {
        let mut displaced = None;
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            if e.path == entry.path && e.stage() == entry.stage() {
                displaced = Some(self.entries.swap_remove(i));
                break;
            }
            i += 1;
        }
        if displaced.is_none() {
            self.entries.push(entry);
        }
        displaced
    }

    /// Phase 3 — remove an entry by path and stage.
    ///
    /// # Caveats
    ///
    /// Removing stage 0 leaves any conflict stages for the same path in place, because they are
    /// different entries. A caller that means "forget this path entirely" must remove all four, which
    /// is what [`IndexExt::remove_path`] is for.
    pub fn remove(&mut self, path: &BStr, stage: Stage) -> Option<IndexEntry> {
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            if e.path == path && e.stage() == stage {
                return Some(self.entries.swap_remove(i));
            }
            i += 1;
        }
        None
    }

    /// Phase 3 — sort the entries into Git's canonical order.
    ///
    /// # Caveats
    ///
    /// The sort must be **stable with respect to equal keys**, so that two entries with the same
    /// path and stage keep their relative order and a duplicate entry (which is corruption) is not
    /// silently swapped — `fsck` should report the duplicate, and a sort that reordered it would
    /// change which one a lookup finds.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.flags.stage().unwrap_or(Stage::Normal).as_bits().cmp(&b.flags.stage().unwrap_or(Stage::Normal).as_bits())))
    }

    /// Phase 3 — whether any path has conflict stages.
    ///
    /// # Caveats
    ///
    /// True when any entry's stage is non-zero. `commit` refuses while this is true, which is why it
    /// is a whole-index question and not a per-path one: a single conflicted path blocks the commit,
    /// exactly as Git's "you have unmerged paths" does.
    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|e| e.flags.stage().unwrap_or(Stage::Normal) != Stage::Normal)
    }

    /// Phase 3 — whether entries are in Git's canonical order.
    ///
    /// # Caveats
    ///
    /// An index that is not sorted is one Git cannot binary-search, so this is a correctness check,
    /// not a tidiness one. It is `O(n)` and is therefore for `fsck`, a test assertion, and the debug
    /// path of [`Index::write`] — not something to run before every lookup.
    pub fn is_sorted(&self) -> bool {
        self.entries.windows(2).all(|w| w[0].path.cmp(&w[1].path).then_with(|| w[0].flags.stage().unwrap_or(Stage::Normal).as_bits().cmp(&w[1].flags.stage().unwrap_or(Stage::Normal).as_bits())) != Ordering::Greater)
    }

    /// Phase 3 — whether an entry's cached stat data is too fresh to trust.
    ///
    /// This is Git's **racy-git** rule. An entry is racy when its mtime is equal to or later than
    /// the index file's own mtime: the file could have been modified within the same filesystem
    /// clock tick that the index was written, in which case its cached size and mtime both match and
    /// the entry looks unchanged while its content is different.
    ///
    /// # Caveats
    ///
    /// * **A racy entry's content must be re-hashed**, not merely re-stat'd. The stat data cannot
    ///   distinguish the two cases by construction — that is what "racy" means — so the only sound
    ///   response is to read the file.
    /// * **`index_mtime` is passed in, not read here.** It comes from the filesystem at the call
    ///   site, and taking it as a parameter is what makes the rule testable without clock control.
    /// * **The comparison includes nanoseconds.** Comparing whole seconds widens the racy window to
    ///   a full second for every entry, which turns a rare re-hash into the common case and makes
    ///   `status` on a large repository read every file.
    /// * **The rule is conservative in one direction only.** An entry that is not racy is still
    ///   assumed unchanged without reading its content, so a filesystem that lies about mtime — or a
    ///   file changed with its mtime and size restored — is missed. Git accepts this; the
    ///   alternative is reading the work tree on every `status`.
    pub fn is_racy(&self, entry: &IndexEntry, index_mtime: (u32, u32)) -> bool {
        entry.mtime_seconds > index_mtime.0 || (entry.mtime_seconds == index_mtime.0 && entry.mtime_nanos >= index_mtime.1)
    }
}

impl Default for Index {
    /// Phase 3 — an empty index; identical to [`Index::new`].
    fn default() -> Self {
        Self {
            version: IndexVersion::V2,
            entries: Vec::new(),
            extensions: Vec::new(),
        }
    }
}

/// Phase 3 — the operations on an index that are not its raw format.
///
/// Kept as an extension trait so that [`Index`] itself stays a description of the file — its header,
/// its entries, its extensions — and the algorithms built on top of it (tree construction, conflict
/// handling, work tree comparison) can be added without the file type growing a method for every
/// caller. It is implemented for [`Index`] and for nothing else.
pub trait IndexExt {
    /// Phase 3 — every stage recorded for one path.
    ///
    /// # Caveats
    ///
    /// Returns 0, 1, or up to 3 entries. Zero means the path is untracked; one stage-0 entry means
    /// resolved; anything else means conflicted — and the distinction between "one stage-1 entry" and
    /// "one stage-0 entry" is the whole of it, so a caller must look at the stages and not just the
    /// length.
    fn stages(&self, path: &BStr) -> Vec<&IndexEntry>;

    /// Phase 3 — whether a path is conflicted.
    ///
    /// # Caveats
    ///
    /// Defined as "has entries and none of them is stage 0", not as "has more than one entry": a
    /// modify/delete conflict leaves two entries, an add/add leaves two, and a path mid-resolution
    /// can briefly have one. `false` for an untracked path, which is why the caller that wants
    /// "unmerged or absent" asks [`IndexExt::stages`] instead.
    fn is_unmerged(&self, path: &BStr) -> bool;

    /// Phase 3 — the paths that are conflicted, sorted.
    ///
    /// # Caveats
    ///
    /// One entry per **path**, not per index entry, so a three-way conflict appears once. This is the
    /// list `commit` refuses on and `status` prints, and its length is the number a user understands —
    /// unlike [`Index::len`], which counts stages.
    fn conflicts(&self) -> Vec<BString>;

    /// Phase 3 — drop every entry for a path, all stages included.
    ///
    /// # Caveats
    ///
    /// This is the "abandon the conflict" operation — `git checkout --ours`/`--theirs` followed by an
    /// `add` uses [`IndexExt::resolve`] instead — and it leaves the path untracked rather than
    /// deleting anything from the work tree.
    fn remove_path(&mut self, path: &BStr) -> Vec<IndexEntry>;

    /// Phase 3 — replace a path's conflict entries with one resolved entry.
    ///
    /// The operation behind `git add` on a conflicted path: whatever stages existed are dropped and
    /// a single stage-0 entry takes their place.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnmergedPath`] is *not* raised here — this is the function that
    /// resolves one. The signature is `Result` because the entry's path must match `path`, and a
    /// mismatch is a caller bug worth reporting rather than an entry filed under the wrong name.
    fn resolve(&mut self, path: &BStr, entry: IndexEntry) -> Result<()>;

    /// Phase 3 — build the tree this index describes.
    ///
    /// Recursive by construction: the entries are a flat list of `/`-separated paths, and a tree is a
    /// hierarchy, so the writer groups by the first path component and recurses. The resulting tree
    /// ids are written to the object database, because a tree refers to its children by id and there
    /// is no way to build the parent without the child's hash.
    ///
    /// # Caveats
    ///
    /// * **Refuses on conflicts.** A conflicted index has no single content for a path, and
    ///   `git write-tree` fails with "unmerged" rather than picking a stage — picking one silently is
    ///   how a merge conflict ends up committed as a resolution.
    /// * **The result must match `git write-tree` byte for byte** for the differential tests, which
    ///   means tree entries sorted by Git's rule (directories compared as if their name ended in
    ///   `/`) and gitlinks written as `160000` entries rather than descended into. That sort is *not*
    ///   [`compare_entries`].
    /// * **Empty directories cannot be represented**, because an index holds only files. A directory
    ///   with no entries simply does not appear, which matches Git: it does not track them either.
    /// * **An empty index produces the empty tree**, whose id is a fixed value Git hard-codes, not an
    ///   error. A fresh repository's first root commit would be unbuildable otherwise.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnmergedPath`] for the first conflicted path found, and the object
    /// database's write errors.
    fn write_tree(&self, odb: &crate::odb::Odb) -> Result<ObjectId>;

    /// Phase 3 — the paths in the index, in file order.
    ///
    /// # Caveats
    ///
    /// One per **entry**, so a conflicted path appears up to three times. `ls-files` prints entries
    /// and is the right model; a caller that wants distinct paths deduplicates.
    fn paths(&self) -> Vec<&BStr>;

    /// Phase 3 — the entries whose path is under a directory, for a pathspec-limited operation.
    ///
    /// # Caveats
    ///
    /// * The prefix is matched on **path components**, not on bytes: `src` must not match
    ///   `srcfile.c`, which a naive `starts_with` does. The comparison is `prefix + "/"` or an exact
    ///   match, which is the rule Git's pathspec matching reduces to for a literal directory.
    /// * An empty prefix means the whole index, which is what `add -A` passes.
    /// * Not recursive in the filesystem sense — it is a filter over the flat entry list, so it costs
    ///   a scan of every entry rather than a walk of one directory.
    fn under(&self, prefix: &BStr) -> Vec<&IndexEntry>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let b = s.replace(' ', "");
        let mut out = Vec::with_capacity(b.len() / 2);
        for i in (0..b.len()).step_by(2) {
            out.push(u8::from_str_radix(&b[i..i + 2], 16).unwrap());
        }
        out
    }

    #[test]
    fn git_entry_bytes_round_trip() {
        let a = unhex(
            "6ad3b1a9 1f0dcc48 6ad3b1a9 1f0dcc48 00000037 000010f3 000081a4 000003e8 000003e8 \
             00000002 587be6b4c3f93f93c489c0111bba5596147a26cb 0001 6100 \
             0000000000000000000000000000000000000000000000000000000000000000",
        );
        let (e, consumed) = IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &a).unwrap();
        assert_eq!(consumed, 64);
        assert_eq!(e.ctime_seconds, 1792258473);
        assert_eq!(e.ctime_nanos, 520997960);
        assert_eq!(e.mtime_seconds, 1792258473);
        assert_eq!(e.mtime_nanos, 520997960);
        assert_eq!(e.dev, 55);
        assert_eq!(e.ino, 4339);
        assert_eq!(e.mode, EntryMode::Blob);
        assert_eq!(e.uid, 1000);
        assert_eq!(e.gid, 1000);
        assert_eq!(e.size, 2);
        assert_eq!(e.oid.to_string(), "587be6b4c3f93f93c489c0111bba5596147a26cb");
        assert_eq!(e.flags.bits(), 0x0001);
        assert_eq!(e.extended_flags, 0);
        assert_eq!(e.path.as_slice(), b"a");
        // `a` carries a 32-byte zero tail from the file it was lifted out of; one entry's
        // serialised form is exactly the bytes the parser consumed.
        assert_eq!(
            e.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap(),
            &a[..consumed]
        );

        let atxt = unhex(
            "6ad3b1a9 1f0dcc48 6ad3b1a9 1f0dcc48 00000037 000010f4 000081a4 000003e8 000003e8 \
             00000006 ce013625030ba8dba906f756967f9e9ca394464a 0005 612e74787400 \
             0000000000000000000000000000000000000000000000000000000000000000",
        );
        let (e, consumed) = IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &atxt).unwrap();
        assert_eq!(consumed, 72);
        assert_eq!(e.ino, 4340);
        assert_eq!(e.size, 6);
        assert_eq!(e.oid.to_string(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(e.flags.bits(), 0x0005);
        assert_eq!(e.path.as_slice(), b"a.txt");
        assert_eq!(
            e.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap(),
            &atxt[..consumed]
        );
    }

    #[test]
    fn flags_words_match_git() {
        assert_eq!(IndexEntryFlags::with(Stage::Normal, 5).bits(), 0x0005);
        assert_eq!(IndexEntryFlags::with(Stage::Theirs, 5).bits(), 0x3005);
        assert_eq!(IndexEntryFlags::empty().set_assume_valid(true).bits(), 0x8000);
        assert_eq!(IndexEntryFlags::empty().set_extended(true).bits(), 0x4000);
        assert!(IndexEntryFlags::from_bits(0x4000).is_extended());
        assert!(IndexEntryExtendedFlags::from_bits(0x4000).skip_worktree());
        assert!(!IndexEntryExtendedFlags::from_bits(0x2000).skip_worktree());
        assert_eq!(IndexEntryExtendedFlags::from_bits(0x1FFF).extended_mode_bits(), 0x1FFF);
        assert_eq!(IndexEntryExtendedFlags::from_bits(0x4001).extended_mode_bits(), 0x0001);
    }

    #[test]
    fn stage_and_version_round_trip() {
        for bits in 0..=3u8 {
            let s = Stage::from_bits(bits).unwrap();
            assert_eq!(s.as_bits(), bits);
            assert_eq!(s.as_number(), bits);
        }
        assert!(matches!(
            Stage::from_bits(4),
            Err(Error::InvalidStage { stage: 4, .. })
        ));
        assert_eq!(IndexVersion::from_u32(2).unwrap(), IndexVersion::V2);
        assert_eq!(IndexVersion::from_u32(3).unwrap(), IndexVersion::V3);
        assert_eq!(IndexVersion::from_u32(4).unwrap(), IndexVersion::V4);
        assert!(matches!(
            IndexVersion::from_u32(5),
            Err(Error::UnsupportedIndexVersion { version: 5, .. })
        ));
        assert_eq!(IndexVersion::V2.as_u32(), 2);
        assert_eq!(IndexVersion::V3.as_u32(), 3);
        assert_eq!(IndexVersion::V4.as_u32(), 4);
        assert!(!IndexVersion::V2.has_extended_flags());
        assert!(IndexVersion::V3.has_extended_flags());
        assert!(!IndexVersion::V4.has_extended_flags());
    }

    #[test]
    fn name_length_saturation_scans_for_nul() {
        let path: BString = "x".repeat(4095).into();
        let entry = IndexEntry {
            ctime_seconds: 1,
            ctime_nanos: 0,
            mtime_seconds: 1,
            mtime_nanos: 0,
            dev: 1,
            ino: 1,
            mode: EntryMode::Blob,
            uid: 0,
            gid: 0,
            size: 4095,
            oid: ObjectId::zero(ObjectFormat::Sha1),
            flags: IndexEntryFlags::with(Stage::Normal, path.len()),
            extended_flags: 0,
            path,
        };
        let bytes = entry.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap();
        let (e, consumed) = IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &bytes).unwrap();
        assert_eq!(e.flags.bits(), 0x0FFF);
        assert_eq!(e.path.len(), 4095);
        assert_eq!(e.path.as_slice(), b"x".repeat(4095));
        let expected = 40 + 20 + 2 + 4095 + 1;
        assert_eq!(consumed, (expected + 7) / 8 * 8);
    }

    #[test]
    fn padding_is_computed_from_entry_start() {
        for len in [1usize, 5, 6, 7] {
            let path = "x".repeat(len).into();
            let entry = IndexEntry {
                ctime_seconds: 1,
                ctime_nanos: 0,
                mtime_seconds: 1,
                mtime_nanos: 0,
                dev: 1,
                ino: 1,
                mode: EntryMode::Blob,
                uid: 0,
                gid: 0,
                size: len as u32,
                oid: ObjectId::zero(ObjectFormat::Sha1),
                flags: IndexEntryFlags::with(Stage::Normal, len),
                extended_flags: 0,
                path,
            };
            let bytes = entry.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap();
            assert_eq!(bytes.len() % 8, 0);
            let (e, consumed) = IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &bytes).unwrap();
            assert_eq!(consumed, bytes.len());
            assert_eq!(e.path.len(), len);
        }
    }

    fn blob_entry(path: &str) -> IndexEntry {
        IndexEntry {
            ctime_seconds: 1,
            ctime_nanos: 0,
            mtime_seconds: 1,
            mtime_nanos: 0,
            dev: 1,
            ino: 1,
            mode: EntryMode::Blob,
            uid: 0,
            gid: 0,
            size: path.len() as u32,
            oid: ObjectId::zero(ObjectFormat::Sha1),
            flags: IndexEntryFlags::with(Stage::Normal, path.len()),
            extended_flags: 0,
            path: path.into(),
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        // Truncated fixed part.
        let short = b"012345678901";
        assert!(IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &short[..])
            .is_err());

        // No NUL terminator.
        let entry = blob_entry("abcd");
        let mut bytes = entry.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap();
        bytes[40 + 20 + 2 + 4] = b'x'; // replace the NUL
        let err = IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &bytes).unwrap_err();
        assert!(matches!(err, Error::CorruptIndex { .. }));

        // Invalid mode.
        let entry = blob_entry("abcd");
        let mut bytes = entry.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap();
        // mode is at bytes 24..28; set it to 010300 octal = 0o10300
        bytes[24..28].copy_from_slice(&0o10300u32.to_be_bytes());
        let err = IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &bytes).unwrap_err();
        assert!(matches!(err, Error::InvalidEntryMode { mode: 0o10300, .. }));

        // Stage is a two-bit field, so 4 is not representable on disk: the byte-level case is
        // that the two bits decode at all, and `Stage::from_bits` rejects 4 directly.
        let entry = blob_entry("abcd");
        let mut bytes = entry.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap();
        // flags word is at 40 + 20 = 60..62; stage 3 with name length 4.
        bytes[60..62].copy_from_slice(&0x3004u16.to_be_bytes());
        let (parsed, _) =
            IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &bytes).unwrap();
        assert_eq!(parsed.flags.stage().unwrap(), Stage::Theirs);
        assert!(matches!(
            Stage::from_bits(4).unwrap_err(),
            Error::InvalidStage { stage: 4 }
        ));
    }

    #[test]
    fn compare_entries_is_memcmp_then_stage() {
        let a_b = blob_entry("a.b");
        let a_slash_b = blob_entry("a/b");
        assert_eq!(compare_entries(&a_b, &a_slash_b), Ordering::Less);

        let s1 = IndexEntry {
            flags: IndexEntryFlags::with(Stage::Base, 3),
            ..blob_entry("abc")
        };
        let s2 = IndexEntry {
            flags: IndexEntryFlags::with(Stage::Ours, 3),
            ..blob_entry("abc")
        };
        assert_eq!(compare_entries(&s1, &s2), Ordering::Less);
        assert_eq!(compare_entries(&s2, &s1), Ordering::Greater);
        assert_eq!(compare_entries(&s1, &s1), Ordering::Equal);
    }

    #[test]
    fn empty_blob_and_modes() {
        let (empty, _) = hash_object(ObjectFormat::Sha1, ObjectKind::Blob, b"").unwrap();
        assert_eq!(
            empty.to_string(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        let mut e = blob_entry("x");
        e.oid = empty;
        assert!(e.is_empty_blob(ObjectFormat::Sha1));
        e.oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            BStr::new("0123456789abcdef0123456789abcdef01234567"),
        )
        .unwrap();
        assert!(!e.is_empty_blob(ObjectFormat::Sha1));

        for (mode, tree, symlink, gitlink) in [
            (EntryMode::Blob, false, false, false),
            (EntryMode::BlobExecutable, false, false, false),
            (EntryMode::Tree, true, false, false),
            (EntryMode::Link, false, true, false),
            (EntryMode::Commit, false, false, true),
        ] {
            let e = IndexEntry { mode, ..blob_entry("x") };
            assert_eq!(e.is_tree(), tree);
            assert_eq!(e.is_symlink(), symlink);
            assert_eq!(e.is_gitlink(), gitlink);
        }
    }

    #[test]
    fn index_extension_classification() {
        let tree = IndexExtension::new(*b"TREE", vec![1, 2, 3]);
        let link = IndexExtension::new(*b"link", vec![]);
        assert!(tree.is_tree());
        assert!(!tree.is_optional());
        assert!(link.is_split_index_link());
        assert!(link.is_optional());
        assert_eq!(tree.signature_str(), "TREE");
        let reuc = IndexExtension::new(*b"REUC", vec![]);
        assert!(reuc.is_reuc());
        let untr = IndexExtension::new(*b"UNTR", vec![]);
        assert!(untr.is_untracked_cache());
        let unknown = IndexExtension::new(*b"\x01\x02\x03\x04", vec![]);
        assert!(!unknown.is_optional());
    }
}
