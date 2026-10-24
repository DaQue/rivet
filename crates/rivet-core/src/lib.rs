//! Rivet — a Git-compatible version control system.
//!
//! Rivet reads and writes **real Git repositories**. It uses `.git/`, Git's object format,
//! Git's index and Git's refs, so `git` and GitHub can operate on the same repository that
//! Rivet is working on. `git fsck` must accept anything Rivet writes.
//!
//! # Verification
//!
//! Behaviour is checked against real `git` rather than against this crate's own opinion of
//! it: `crates/rivet-core/tests/*_git_differential.rs` and `tools/difftest/` run the same
//! operation through both tools and compare the repositories and the output.
//!
//! # Example
//!
//! ```no_run
//! use rivet_core::Repository;
//!
//! let repo = Repository::open()?;
//! if let Some(head) = repo.head_commit()? {
//!     println!("{head}");
//! }
//! # Ok::<(), rivet_core::Error>(())
//! ```
//!
//! `Repository::init` and `Repository::open` are the two entry points; everything else hangs off
//! the returned `Repository` or off free functions in the modules below.
//!
//! # Quick start
//!
//! Write a blob into a fresh repository and read it back — the object id is the same one `git
//! hash-object` computes, because this is a real repository:
//!
//! ```
//! use rivet_core::{InitOptions, ObjectKind, Repository};
//!
//! let dir = tempfile::tempdir().unwrap();
//! let repo = Repository::init(dir.path(), &InitOptions::new())?;
//!
//! let oid = repo.odb().write_raw(ObjectKind::Blob, b"hello, world\n")?;
//! assert_eq!(oid.to_string(), "4b5fa63702dd96796042e92787f464e28f09f17d");
//!
//! let object = repo.odb().read(&oid)?;
//! assert_eq!(object.data(), b"hello, world\n");
//! # Ok::<(), rivet_core::Error>(())
//! ```
//!
//! # Layout
//!
//! * [`object`] — blob, tree, commit and tag: parsing, serialising, hashing.
//! * [`odb`] — the object database, loose and packed.
//! * [`pack`] — `.pack` / `.idx` files, delta resolution, the pack writer.
//! * [`index`] — the staged snapshot in `.git/index`.
//! * [`refs`] — branches, tags, `HEAD`, reflogs, `packed-refs`.
//! * [`repo`] — repository discovery, `init`, configuration lookup.
//! * [`worktree`] — the checkout side: status, add, rm, checkout plans.
//! * [`revwalk`] / [`revparse`] — commit traversal and revision-syntax resolution.
//! * [`diff`] / [`merge`] — content and tree comparison, three-way merges.
//! * [`remote`] / [`remote_ops`] — transports, the wire protocol, fetch and push.
//! * [`ignore`], [`config`], [`fsck`], [`ops`] — pathspec matching, configuration, integrity
//!   checking, and the porcelain operations built on the layers above.
//!
//! # Byte strings, not strings
//!
//! Everything that comes off disk — paths, ref names, object payloads, config values — is
//! `bstr::BString` / `&BStr` / `&[u8]`. Git paths are not necessarily UTF-8, and neither are
//! commit messages (see the `encoding` commit header). `String` is reserved for human-facing
//! text that Rivet constructs itself, and `&str` for static literals.
//!
//! # Object formats
//!
//! SHA-1 is the default, because GitHub only accepts SHA-1 publicly today; SHA-256 is
//! supported end to end. No code in this crate may hard-code 20 bytes or
//! 40 hex characters — always go through [`ObjectFormat`] and [`ObjectId`]. Retrofitting
//! format-genericity later is the expensive part, so it is baked in from the first signature.
//!
//! # Errors
//!
//! Every parser returns [`Result`] with a specific [`Error`] variant. Rivet never panics on
//! repository data: malformed input produces an error, not a panic. There is exactly one error
//! type in the core; `anyhow` is confined to the CLI.
//!
//! # Synchronous by design
//!
//! The core has no `async`. Network I/O in `remote` uses blocking transports.
//!
//! # Stability
//!
//! Pre-1.0: items may move or change between minor versions. What is stable is the part that
//! has to be — Git's on-disk and on-the-wire formats, which are the compatibility contract
//! described above, not this crate's API.

#![warn(missing_docs)]

pub mod archive;
pub mod bundle;
pub mod config;
pub mod diff;
pub mod error;
pub mod fsck;
pub mod gc;
pub mod hash;
pub mod ignore;
pub mod index;
pub mod merge;
pub mod object;
pub mod odb;
pub mod ops;
pub mod pack;
pub mod refs;
pub mod remote;
pub mod remote_ops;
pub mod repo;
pub mod revparse;
pub mod revwalk;
pub mod worktree;

// `repo::Repository` deliberately has no `clone` / `fetch` / `pull` / `push` entry points yet.
// Each of them returns a type that `remote` owns — `FetchOutcome`, `PushOutcome`, `CheckoutPlan`
// — and the CLI drives `remote::fetch` / `remote::push` against a transport it opened itself.

pub use crate::config::{
    Config, ConfigEntry, ConfigOrigin, config_env_override, parse_bool, parse_int,
};
pub use crate::diff::{
    DiffAlgorithm, DiffLine, DiffOptions, DiffStat, Edit, FileDiff, Hunk, HunkLine, LineKind,
    RenamePair, RenameSide, TreeDiff, blob_diff, change_letter, detect_renames, entry_diff,
    format_name_status, format_stat, format_stat_sized, format_unified, histogram_diff, index_diff,
    index_line_abbrev, is_binary, line_count, myers_diff, pair_renames, pprint_rename, split_lines,
    tree_diff, work_tree_diff, write_unified,
};
pub use crate::error::{Error, Result};
pub use crate::hash::{ObjectFormat, ObjectHasher, ObjectId};
pub use crate::ignore::{
    IgnoreDecision, IgnoreFrame, IgnoreSource, IgnoreStack, PathKind, Pattern,
};
pub use crate::index::{
    DIRC_MAGIC, INDEX_FILE, Index, IndexEntry, IndexEntryExtendedFlags, IndexEntryFlags, IndexExt,
    IndexExtension, IndexVersion, MAX_INDEX_VERSION, Stage, compare_entries,
};
pub use crate::merge::{
    Conflict, ConflictHunk, ConflictKind, ConflictStyle, ContentMerge, MAX_MERGE_BASE_STEPS,
    MERGE_HEAD, MERGE_MSG, MergeBase, MergeOptions, MergeOutcome, MergeSide, MergeState, ORIG_HEAD,
    PathMerge, TreeMerge, abort_merge, clear_merge_state, merge_base, merge_bases, merge_content,
    merge_in_progress, merge_path, merge_trees, merge_worktree_paths, read_merge_state,
    write_conflict_stages, write_merge_state, write_merged_worktree, write_orig_head,
};
pub use crate::object::{
    AnyObject, Blob, Commit, CommitBuilder, EntryMode, Object, ObjectKind, Signature, Tag, Tree,
    TreeEntry, hash_object,
};
pub use crate::odb::{LooseObjectStore, Odb, PackedObjectStore};
pub use crate::pack::{
    BaseProvider, DeltaBase, DeltaHeader, EntryKind, IndexPackResult, MAX_DELTA_DEPTH,
    OwnedPackFile, OwnedPackIndex, PackEntry, PackFile, PackIndex, PackIndexEntry, PackIndexIter,
    PackReader, PackWriteSummary, PackWriter, apply_delta, fix_thin_pack, index_pack,
    parse_delta_header, resolve_delta,
};
pub use crate::refs::packed::{HEADER_PREFIX, packed_names};
pub use crate::refs::{
    GitDirPaths, HEAD, HeadState, MAX_SYMBOLIC_DEPTH, PackedRef, PackedRefs, RefCategory,
    RefLocation, RefLock, RefName, RefStore, RefTarget, ReflogEntry, ReflogMessage,
    is_valid_ref_name, packed_refs_path,
};
pub use crate::remote::{
    Ack, AdvertisedRef, Capability, CapabilitySet, ChannelWrite, DEFAULT_TIMEOUT,
    FetchCapabilities, FetchOptions, FetchOutcome, FetchRefResult, FetchRefUpdate, FetchRefs,
    FetchSpec, HAVES_PER_ROUND, HttpRequest, HttpResponse, HttpTransport, LocalTransport,
    MAX_NEGOTIATION_ROUNDS, MAX_PKT_DATA, MAX_SIDEBAND_DATA, Negotiation, PktLine, ProtocolVersion,
    PushOutcome, PushSpec, PushStatus, RefAdvertisement, RefUpdate, RemoteScheme, RemoteUrl,
    Service, SidebandKind, SidebandOptions, SidebandPacket, SidebandReader, SshTransport,
    StoredPack, Transport, USER_AGENT, check_updates, credential_span, decode_body, demultiplex,
    fetch, guess_remote_name, is_auth_challenge, is_fast_forward, is_success, is_up_to_date,
    local_haves, media_type, missing_from, negotiate, parse_ack, parse_report_status, push,
    push_capabilities, push_is_fast_forward, push_request, quote_remote_path, redact_url,
    remote_command, ssh_args, ssh_command_for, store_pack, stream_pack, tracking_ref_name,
    update_commands, wants_for_tracking, wants_request, write_tracking_refs,
};
pub use crate::repo::{
    CommitOptions, GIT_DIR_ENV, GIT_WORK_TREE_ENV, InitOptions, Repository, RepositoryKind,
    find_git_dir, gitfile_target,
};
pub use crate::revwalk::{
    CommitWalker, RevRange, RevSpec, Revision, SortOrder, WalkOptions, WalkTerminator,
    parse_rev_spec,
};
pub use crate::worktree::{
    AddOptions, AddOutcome, ChangeKind, CheckoutAction, CheckoutEntry, CheckoutOptions,
    CheckoutPlan, RmOptions, StatCache, StatData, Status, StatusEntry, StatusOptions,
    UntrackedMode, Worktree, WorktreeEntry, WorktreeWalkOptions,
};
