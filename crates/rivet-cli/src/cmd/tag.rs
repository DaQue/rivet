//! The `tag` command: list, create, verify and delete tags.
//!
//! Phase 7, and the smaller half of milestone **M2**. A tag is either a *lightweight* ref under
//! `refs/tags/` that points straight at an object, or an *annotated* `tag` object that points at
//! one and carries a tagger, a message and — optionally — a signature. Both live in the same
//! namespace and both are peeled the same way by a revision parser.

use anyhow::Result;

use crate::cli::GlobalArgs;

/// Arguments to `rivet tag`.
///
/// # Caveats
///
/// * **The command has five modes and the mode is decided by the flags, in this order:**
///   `-d` deletes, `-v` verifies, `-l` lists, an `object` positional creates, and neither a
///   name nor any of those lists. A `-v` implemented as "create a tag named `-v`" is the failure
///   mode of getting this wrong; `-v` is declared long-only here precisely because `-v` is the
///   *global* `--verbose` and a subcommand cannot take it; see [`crate::cli::GlobalArgs`].
/// * **`-l` with no pattern lists every tag, and `-l` with a pattern is a shell glob, not a
///   regular expression.** `v1.*` matches `v1.0`; `v1.*` as a regex would match `v1x0`. The two
///   are close enough that getting it wrong is not noticed until a tag with a literal `.` in it
///   disappears from the listing.
/// * **An annotated tag requires `-a`, `-s` or `-u`**, and creating one with a message but
///   without `-a` is Git's `fatal: no tag message?`. A lightweight tag ignores `-m` entirely
///   rather than silently annotating, because the two produce different object ids and a user
///   who wanted a lightweight tag must get the id they expected.
/// * **`-f` replaces an existing tag and rewrites its reflog.** An annotated tag that is
///   replaced is a different object at the same name, and every clone that already has the old
///   one now disagrees about what `v1.0` means. Git warns loudly; so must Rivet, on stderr, even
///   though the command succeeds.
/// * **`-d` deletes the ref, and for an annotated tag that leaves the tag object unreferenced**
///   — recoverable from the reflog for the ref's own lifetime and no longer. That is correct and
///   is not a reason to prompt.
/// * **`--verify` checks the signature and exits non-zero when the tag is unsigned.** An
///   unsigned tag under `-v` is exit `1`, not `128`: the tag exists and the answer to "is it
///   signed" is no.
/// * **`--points-at` and `--merged` are filters on the listing**, and they compose with `-l`
///   rather than replacing it — `rivet tag -l --merged main` lists tags reachable from `main`.
#[derive(Debug, clap::Args)]
pub struct TagArgs {
    /// The tag to create, delete, verify or describe.
    #[arg(value_name = "TAG")]
    pub name: Option<String>,

    /// The object to tag. Defaults to `HEAD`.
    #[arg(value_name = "COMMIT")]
    pub object: Option<String>,

    /// Create an annotated tag object rather than a lightweight ref.
    #[arg(short = 'a', long = "annotate")]
    pub annotate: bool,

    /// The tag message. Implies `--annotate`.
    #[arg(short = 'm', long = "message", value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Read the tag message from a file, or from standard input with `-`.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    pub file: Option<std::path::PathBuf>,

    /// Sign the tag, or with no key name the configured default. Not implemented in v1.
    #[arg(short = 's', long = "sign")]
    pub sign: bool,

    /// Sign the tag with this key. Not implemented in v1.
    #[arg(short = 'u', long = "local-user", value_name = "KEY")]
    pub local_user: Option<String>,

    /// Verify the tag's signature.
    #[arg(long = "verify")]
    pub verify: bool,

    /// Delete the tag.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// List tags whose name matches this shell pattern.
    #[arg(short = 'l', long = "list", value_name = "PATTERN")]
    pub list: Option<Option<String>>,

    /// Replace an existing tag.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Print the tag message and the commit it points at.
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// List only the tags pointing at this object.
    #[arg(long = "points-at", value_name = "OBJECT")]
    pub points_at: Option<String>,

    /// List only the tags whose commits are reachable from this one.
    #[arg(long = "merged", value_name = "COMMIT")]
    pub merged: Option<Option<String>>,

    /// List only the tags whose commits are not reachable from this one.
    #[arg(long = "no-merged", value_name = "COMMIT")]
    pub no_merged: Option<Option<String>>,

    /// List the tags whose commits are contained in this one.
    #[arg(long = "contains", value_name = "COMMIT")]
    pub contains: Option<String>,

    /// Print this many lines of the tag message in the listing.
    #[arg(short = 'n', value_name = "NUMBER")]
    pub lines: Option<Option<usize>>,

    /// Sort the listing by this key rather than by name.
    #[arg(long = "sort", value_name = "KEY")]
    pub sort: Option<String>,

    /// List the tags in the order Git stores them rather than sorted by name.
    #[arg(long = "ignore-case")]
    pub ignore_case: bool,

    /// Print the tag names quoted for a shell.
    #[arg(long = "column", value_name = "WHEN")]
    pub column: Option<Option<String>>,

    /// Count the tags rather than listing them.
    #[arg(long = "omit-empty")]
    pub omit_empty: bool,
}

/// Phase 7 — list, create, verify and delete tags.
///
/// # Purpose
///
/// Create a lightweight or annotated tag, list the ones that exist, print one tag's object, or
/// delete it. Annotated tags are written as real `tag` objects with a tagger and a message, so
/// that `git show v1.0` displays what Rivet wrote.
///
/// # Exit codes
///
/// * `0` — the tag was created, deleted, listed or displayed.
/// * `1` — `-v` on an unsigned or badly signed tag, or `-l <pattern>` matching nothing. The
///   requested negative in both cases.
/// * `128` — the name is not a legal ref name, the tag exists without `--force`, the object does
///   not resolve, `-d` names a tag that is not there, or signing was requested and is not
///   implemented.
///
/// # Caveats
///
/// * **A lightweight tag and an annotated tag have different object ids for the same name**, and
///   only the annotated one is an object. `rivet tag v1.0` creates a ref; `rivet tag -a v1.0 -m
///   msg` creates an object and points a ref at it. A handler that annotated by default would
///   produce a different id from `git tag v1.0` and break every differential test that compares
///   them.
/// * **The tagger identity comes from the config and its absence is an error**, exactly as for a
///   commit: a tag object with an invented tagger is one the user cannot fix without rewriting
///   the tag.
/// * **The message is written byte for byte, and Git appends a trailing newline if there is
///   none.** For a commit, normalising is wrong because `git commit` does not; for a tag, Git
///   *does* clean up the message, so matching `git tag -a` means doing what it does — which is
///   the opposite rule from [`crate::cmd::snapshot::run_commit`], and a shared "message" helper
///   between the two would get one of them wrong.
/// * **`-m` implies `-a`, and a message without `-a` is only legal for a lightweight tag when
///   the message is empty.** Git's `fatal: no tag message?` comes from the other direction —
///   `-a` with no message — which is the case that must open an editor or fail, and never
///   silently create a tag with an empty message.
/// * **Deleting a tag also deletes its reflog**, which is `refs/tags/<name>`'s log file and not
///   the tag object. The object stays until a `gc`, and that is deliberate: a tag deleted by
///   mistake is recoverable until then and not after.
/// * **`--points-at` peels annotated tags before comparing**, so `refs/tags/v1` that points at a
///   tag object whose target is commit `abc` *does* match `--points-at abc`. Comparing the ref's
///   direct target only reports it as pointing at a tag object, which is not what the user
///   asked.
/// * **The listing is sorted by name by default and `--sort` changes that**, but a listing that
///   mixes local and packed tags must sort the merged result, not concatenate loose-then-packed.
///   Tags are the refs most likely to have been packed by a `git gc`, so this is the common
///   case, not an edge one.
pub fn run_tag(global: &GlobalArgs, args: &TagArgs) -> Result<i32> {
    todo!()
}
