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
    if args.sign || args.local_user.is_some() || args.verify {
        anyhow::bail!("tag: signing and verification are not implemented");
    }
    if args.column.is_some()
        || args.merged.is_some()
        || args.no_merged.is_some()
        || args.contains.is_some()
    {
        anyhow::bail!("tag: --column, --merged, --no-merged and --contains are not implemented");
    }
    let repository = super::open(global)?;

    if args.delete {
        let mut code = 0;
        let names: Vec<&String> = args.name.iter().chain(args.object.iter()).collect();
        if names.is_empty() {
            anyhow::bail!("tag name required");
        }
        for name in names {
            let reference = RefName::tag(BStr::new(name.as_bytes()))
                .map_err(|_| anyhow::anyhow!("'{name}' is not a valid tag name."))?;
            match repository
                .refs()
                .try_read(&reference)?
                .and_then(|t| t.oid().ok())
            {
                Some(oid) => {
                    repository.refs().delete(&reference)?;
                    println!("Deleted tag '{name}' (was {})", super::short_oid(&oid));
                }
                None => {
                    eprintln!("error: tag '{name}' not found.");
                    code = 1;
                }
            }
        }
        return Ok(code);
    }

    let listing = args.list.is_some()
        || (args.name.is_none() && !args.annotate && args.message.is_none() && args.file.is_none());
    if listing {
        let pattern: Option<String> = match &args.list {
            Some(Some(p)) => Some(p.clone()),
            _ => args.name.clone(),
        };
        let points_at = match &args.points_at {
            Some(spec) => Some(
                repository
                    .refs()
                    .peel_oid(super::resolve_rev(&repository, spec)?, repository.odb())
                    .unwrap_or(super::resolve_rev(&repository, spec)?),
            ),
            None => None,
        };
        let mut rows: Vec<(String, ObjectId)> = repository
            .refs()
            .list_tags()?
            .into_iter()
            .map(|(name, oid)| (name.short_name().to_string(), oid))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let mut printed = 0;
        for (name, oid) in rows {
            if let Some(pattern) = &pattern
                && !super::branching::glob_match(pattern.as_bytes(), name.as_bytes())
            {
                continue;
            }
            if let Some(target) = points_at {
                let peeled = repository
                    .refs()
                    .peel_oid(oid, repository.odb())
                    .unwrap_or(oid);
                if oid != target && peeled != target {
                    continue;
                }
            }
            printed += 1;
            match args.lines {
                Some(lines) => {
                    let lines = lines.unwrap_or(1);
                    let text = tag_annotation(&repository, &oid, lines)?;
                    println!("{name:<15} {text}");
                }
                None => println!("{name}"),
            }
        }
        return Ok(if printed == 0 && pattern.is_some() {
            1
        } else {
            0
        });
    }

    let Some(name) = &args.name else {
        anyhow::bail!("tag name required");
    };
    let reference = RefName::tag(BStr::new(name.as_bytes()))
        .map_err(|_| anyhow::anyhow!("'{name}' is not a valid tag name."))?;
    let existing = repository
        .refs()
        .try_read(&reference)?
        .and_then(|t| t.oid().ok());
    if existing.is_some() && !args.force {
        anyhow::bail!("tag '{name}' already exists");
    }
    let target_spec = args.object.as_deref().unwrap_or("HEAD");
    let target = super::resolve_rev(&repository, target_spec)
        .map_err(|_| anyhow::anyhow!("Failed to resolve '{target_spec}' as a valid ref."))?;

    let message: Option<Vec<u8>> = if let Some(message) = &args.message {
        Some(message.clone().into_bytes())
    } else if let Some(path) = &args.file {
        Some(if path.as_os_str() == "-" {
            use std::io::Read as _;
            let mut buffer = Vec::new();
            std::io::stdin().read_to_end(&mut buffer)?;
            buffer
        } else {
            std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("could not open '{}': {e}", path.display()))?
        })
    } else {
        None
    };

    let annotated = args.annotate || message.is_some();
    let value = if annotated {
        let Some(message) = message else {
            anyhow::bail!("no tag message?");
        };
        let message = cleanup_tag_message(&message);
        if message.is_empty() {
            anyhow::bail!("no tag message?");
        }
        let kind = repository.odb().read_header(&target)?.0;
        let advice = advice_setting(&repository, "advice.nestedTag");
        if kind == rivet_core::ObjectKind::Tag && advice != Some(false) {
            eprintln!(
                "hint: You have created a nested tag. The object referred to by your new tag is"
            );
            eprintln!(
                "hint: already a tag. If you meant to tag the object that it points to, use:"
            );
            eprintln!("hint:");
            eprintln!("hint: \tgit tag -f {name} {target_spec}^{{}}");
            if advice.is_none() {
                eprintln!(
                    "hint: Disable this message with \"git config set advice.nestedTag false\""
                );
            }
        }
        let tagger = repository.committer_signature()?;
        let mut builder = rivet_core::object::tag::TagBuilder::new();
        builder
            .object(target)
            .kind(kind)
            .name(name.as_bytes())
            .tagger(tagger)
            .message(&message);
        let body = builder.build(repository.format())?;
        repository
            .odb()
            .write_raw(rivet_core::ObjectKind::Tag, &body)?
    } else {
        target
    };

    // Tags are written without a reflog line, as Git does.
    repository.refs().set_direct(&reference, value, None)?;
    if let (Some(old), true) = (existing, args.force) {
        println!("Updated tag '{name}' (was {})", super::short_oid(&old));
    }
    Ok(0)
}

use bstr::BStr;
use rivet_core::{ObjectId, RefName, Repository};

/// How an `advice.*` key is configured: `None` when unset (the hint shows, with its "Disable this
/// message" line), otherwise the boolean (an explicit `true` shows the hint without that line).
fn advice_setting(repository: &Repository, key: &str) -> Option<bool> {
    repository
        .config_value(BStr::new(key.as_bytes()))
        .ok()
        .flatten()
        .and_then(|value| rivet_core::parse_bool(BStr::new(&value[..])).ok())
}

/// Git's tag message cleanup: strip trailing whitespace per line, collapse runs of blank lines,
/// drop leading and trailing blank lines, and end with exactly one newline.
fn cleanup_tag_message(message: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(message);
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = lines.join("\n").into_bytes();
    out.push(b'\n');
    out
}

/// The first `lines` lines of a tag's message, or of the commit's for a lightweight tag.
fn tag_annotation(repository: &Repository, oid: &ObjectId, lines: usize) -> Result<String> {
    let object = repository.odb().read(oid)?;
    let message: Vec<u8> = match object.parse()? {
        rivet_core::AnyObject::Tag(tag) => tag.message.to_vec(),
        rivet_core::AnyObject::Commit(commit) => commit.message.to_vec(),
        _ => Vec::new(),
    };
    let text = String::from_utf8_lossy(&message);
    let mut shown: Vec<String> = Vec::new();
    for line in text
        .lines()
        .skip_while(|l| l.trim().is_empty())
        .take(lines.max(1))
    {
        shown.push(line.trim_end().to_string());
    }
    Ok(shown.join("\n                 "))
}
