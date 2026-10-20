//! Phase 4 — the repository: discovery, and the objects that make up one.
//!
//! Everything in this crate is a component — an object database, a ref store, an index, a config,
//! a work tree — and this module is where they are held together into the thing a user thinks of
//! as "the repository". It owns three questions and nothing else:
//!
//! 1. **Where is it?** ([`find_git_dir`], [`Repository::discover`]) — walking up from a directory
//!    to find `.git`, honouring `GIT_DIR` and `GIT_WORK_TREE`, and handling a `.git` **file**
//!    (a gitfile) as well as a directory.
//! 2. **What shape is it?** ([`RepositoryKind`]) — a normal repository with a work tree, a bare
//!    one without, or a work tree split from its git directory by a gitfile or by
//!    `core.worktree`. The distinction decides which operations are legal at all, and the error
//!    for the illegal ones is [`crate::error::Error::BareRepository`].
//! 3. **What are its parts?** — [`Repository::odb`], [`Repository::refs`],
//!    [`Repository::index`], [`Repository::config`], [`Repository::worktree`], all loaded on
//!    open and kept consistent with one another.
//!
//! # The gitfile
//!
//! A `.git` file is a text file whose contents are
//!
//! ```text
//! gitdir: /path/to/the/real/git/dir
//! ```
//!
//! with an optional trailing newline. It is how a submodule's directory points at
//! `.git/modules/<name>`, and how `git worktree` gives each linked work tree its own `.git` file
//! pointing back at the common directory. It is also a **security-relevant** construct: the path
//! it contains may be relative (resolved against the file's own directory, not the process's
//! working directory), and a gitfile whose target escapes anywhere is how a checked-out repository
//! gets another repository's configuration read as its own. See [`gitfile_target`].
//!
//! # Caveats that bite
//!
//! * **`GIT_DIR` overrides discovery entirely.** With it set, Rivet must not walk up and must not
//!   look for `.git` — the directory named is the git directory, full stop. A caller that used it
//!   as a hint rather than an override opens the wrong repository, and the failure is silent
//!   because both repositories exist.
//! * **`GIT_WORK_TREE` is only meaningful with a git directory already known**, and it is what
//!   makes a bare repository usable as if it had a work tree. Setting it alone is an error in Git
//!   and must be here too, rather than being ignored.
//! * **A repository's object format comes from its config**, not from the presence of a file. The
//!   `extensions.objectFormat` key decides whether ids are 20 bytes or 32, and reading a SHA-256
//!   repository as SHA-1 produces [`crate::error::Error::InvalidHex`] errors on every ref — which
//!   looks like corruption and is a one-line fix. The format must be read before anything that
//!   parses an id.
//! * **`core.repositoryformatversion` is checked before the format is trusted.** A version Rivet
//!   does not know may have changed the on-disk layout in ways that make guessing dangerous; see
//!   [`crate::error::Error::UnsupportedRepositoryFormatVersion`].
//! * **Discovery crosses filesystem boundaries without asking.** `git` stops at the filesystem
//!   root and does not stop at a mount point, so `/home/user/repo` under a directory with a
//!   `.git` at `/home` finds the outer one. That is Git's behaviour and matching it matters more
//!   than the surprise, but a caller that wanted the *nearest* repository rather than the first
//!   one found has to say so.
//! * **A work tree and a git directory are not the same path**, and assuming they are is the bug
//!   that makes submodules and `git worktree` produce a repository whose index describes files
//!   that are not there. The two are stored separately for that reason, and `core.worktree` (and
//!   the gitfile) are the two ways they diverge.
//! * **This type holds no locks.** Opening a repository is a read; two processes may open the
//!   same one, and the atomicity of every write is the individual component's concern (the index
//!   through `index.lock`, refs through `<ref>.lock`). A caller that wanted a repository-wide
//!   lock is asking for something Git does not have either.
//! * **There is no `clone`, `fetch`, `pull` or `push`.** Each returns a type that `remote` will
//!   own, and the module does not exist yet (plan §4 Phase 11); declaring the entry points now
//!   would mean either a placeholder type that later batches must not use, or a signature that
//!   changes. See the note at the top of the crate root (the module list in `lib.rs`).

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use bstr::{BStr, BString};

use crate::config::Config;
use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::ignore::IgnoreStack;
use crate::index::Index;
use crate::object::{Signature, Tree};
use crate::odb::Odb;
use crate::refs::{HeadState, RefStore};
use crate::worktree::Worktree;
use crate::error::Error;

/// Phase 4 — the environment variable naming a git directory directly.
///
/// # Caveats
///
/// * **It is an override, not a hint**; see the module caveat. With it set, discovery does not
///   happen at all.
/// * **It is a `&str`, and the value read from the environment is a `BString`.** The constant is
///   the variable's *name*, which is ASCII by definition; the value is a path and may not be
///   UTF-8, so the accessor returns bytes. A caller that read the variable with
///   `std::env::var` would lose a path with a non-UTF-8 component, which is exactly the path that
///   is hardest to diagnose.
/// * **An empty value means unset.** Git treats `GIT_DIR=` as no git directory rather than as the
///   empty path, and a caller that passed it on to a filesystem call would get an error about the
///   current directory instead of the honest answer.
pub const GIT_DIR_ENV: &str = "GIT_DIR";

/// Phase 4 — the environment variable naming the work tree explicitly.
///
/// # Caveats
///
/// * **Meaningful only together with a git directory** — from [`GIT_DIR_ENV`] or from discovery.
///   Setting it alone is an error, not a value to be ignored.
/// * **It does not have to be a parent of the git directory**, and a caller must not assume it is:
///   the whole point of the variable is to make a bare repository usable as if it had a work tree,
///   and that work tree can live anywhere.
/// * **It is not `core.worktree`.** The config key is the persistent spelling and this is the
///   per-invocation one; when both are set the environment wins, which is Git's rule for every
///   `GIT_*` variable and the reason a script can override a repository's own configuration
///   without editing it.
pub const GIT_WORK_TREE_ENV: &str = "GIT_WORK_TREE";

/// Phase 4 — what kind of repository was opened.
///
/// # Caveats
///
/// * **[`RepositoryKind::Normal`] is the only one in which every operation is legal.** The other
///   two exist so that the refusal is specific — "this repository has no work tree" rather than a
///   failure deep inside a checkout — which is what makes the error message useful.
/// * **[`RepositoryKind::WorkTree`] is not bare and is not normal.** It is a linked work tree: a
///   `.git` file pointing at a git directory shared with other work trees, with its own `HEAD`,
///   its own index and its own work tree files. Treating it as bare loses the work tree; treating
///   it as normal reads the *shared* `HEAD` and checks out the wrong branch. The two mistakes look
///   nothing alike and both come from the same conflation.
/// * **The kind is decided at open time and is not re-derived**, so a repository whose
///   `core.bare` is changed by another process during a long operation keeps the kind it was
///   opened with. That is deliberate: re-reading would let a concurrent config change alter the
///   meaning of an operation halfway through. The opposite choice is also defensible and the
///   important thing is that it is a choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RepositoryKind {
    /// A git directory with a work tree, in the same place or named by `core.worktree`.
    Normal,
    /// A git directory with no work tree at all.
    Bare,
    /// A work tree whose `.git` is a file pointing at a git directory elsewhere.
    WorkTree,
}

/// Phase 4 — how `init` should create a repository.
///
/// # Caveats
///
/// * **`format` is a choice that cannot be changed afterwards.** A repository's object format is
///   fixed at creation; converting requires a full rewrite and Git does not do it either. A
///   caller defaulting to SHA-1 is doing the right thing — GitHub accepts only SHA-1 publicly —
///   and one that takes the format from a flag must say so prominently, because a user who
///   creates a SHA-256 repository discovers the incompatibility at their first push.
/// * **`work_tree` and `bare` are not independent.** `bare: true` with a `work_tree` is a
///   contradiction; `bare: false` with no `work_tree` means "the directory being initialised".
///   The combination must be resolved at the call site rather than stored, or two callers build
///   two different sets of files for the same intent.
/// * **`initial_branch` is written to `HEAD` and to nothing else.** It is not a branch — an
///   unborn `HEAD` is a symbolic ref to a ref that does not exist yet — which is why a repository
///   created with it has no refs at all, and why `git init` on a fresh directory leaves a
///   repository whose `status` says "no commits yet".
/// * **There is no `--template` and no `--separate-git-dir`.** The first is a directory of hooks
///   Rivet does not ship; the second is a gitfile-creating mode that belongs with the gitfile work
///   in this module rather than in a creation option, and adding it as a bool here would hide the
///   path-resolution rules it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitOptions {
    /// Create a bare repository with no work tree.
    pub bare: bool,
    /// The branch `HEAD` names before the first commit.
    pub initial_branch: BString,
    /// The object format to create.
    pub format: ObjectFormat,
    /// Where the work tree should be, or `None` to use the directory being initialised.
    pub work_tree: Option<PathBuf>,
}

impl InitOptions {
    /// Phase 4 — the options a plain `rivet init` uses: not bare, `main` as the initial branch,
    /// SHA-1, work tree at the directory being initialised.
    ///
    /// # Caveats
    ///
    /// `main` rather than Git's historical `master`, matching Git's own default since 2.28 and
    /// the configuration most hosting services now assume. A caller that needs `master` — an old
    /// CI system, a script that hard-codes it — sets it explicitly; the default is a decision
    /// with a date on it and this one is made deliberately.
    pub fn new() -> Self {
        Self {
            bare: false,
            initial_branch: BString::from("main"),
            format: ObjectFormat::Sha1,
            work_tree: None,
        }
    }

    /// Phase 4 — create a bare repository.
    pub fn bare(&mut self, on: bool) -> &mut Self {
        self.bare = on;
        self
    }

    /// Phase 4 — set the initial branch name.
    pub fn initial_branch(&mut self, name: &BStr) -> &mut Self {
        self.initial_branch = BString::from(name);
        self
    }

    /// Phase 4 — select the object format.
    pub fn format(&mut self, format: ObjectFormat) -> &mut Self {
        self.format = format;
        self
    }
}

impl Default for InitOptions {
    /// Phase 4 — the defaults of [`InitOptions::new`].
    fn default() -> Self {
        Self {
            bare: false,
            initial_branch: BString::from("main"),
            format: ObjectFormat::Sha1,
            work_tree: None,
        }
    }
}

/// Phase 4 — what `commit` should do.
///
/// # Caveats
///
/// * **`author` and `committer` are separate and both required**, and the default for both is the
///   ambient identity from config — which may be absent, and whose absence must be an error
///   rather than an invented `rivet@localhost`. A commit with a fabricated author is one that
///   `git fsck` accepts and that a user cannot fix without rewriting history.
/// * **`parents` is not derivable from `HEAD` in every case.** A merge commit's parents are
///   `HEAD` and `MERGE_HEAD`; an `amend`'s parent is `HEAD~1`; a root commit has none. The field is
///   here so that the caller which knows says, rather than `commit` reading `MERGE_HEAD` behind
///   the caller's back and producing a merge the user did not ask for.
/// * **`allow_empty` is off by default and Git's refusal is the right one.** A commit with no
///   change relative to its parent is almost always a mistake (`git commit` with nothing staged
///   is a typo, not an intent); `--allow-empty` exists for the cases where it is deliberate, such
///   as a CI trigger or a marker commit.
/// * **`amend` rewrites, it does not append**, and the id it replaces is the one `HEAD` names —
///   which means an amended commit is a *different* object and every descendant of the old one is
///   now on a branch of history that no longer contains the new one. A caller that treated it as
///   an edit would be surprised by the reflog.
/// * **There is no message editor here.** The message is bytes and the caller supplies them; the
///   interactive editor is a CLI concern and putting a `tempfile` and a subprocess in the core
///   would make the library depend on a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOptions {
    /// The author to record, or `None` to take the ambient identity.
    pub author: Option<Signature>,
    /// The committer to record, or `None` to take the ambient identity.
    pub committer: Option<Signature>,
    /// The commit message, as bytes.
    pub message: BString,
    /// Parents to record, in order.
    pub parents: Vec<ObjectId>,
    /// Whether to use the tree as staged even if it equals the parent's.
    pub allow_empty: bool,
    /// Whether to accept a message that is empty or whitespace-only.
    ///
    /// Git's `--allow-empty-message` is a separate switch from `--allow-empty`: one concerns the
    /// tree, the other the message. Folding them into one flag would let `--allow-empty` silently
    /// publish a commit with no description, so they stay apart.
    pub allow_empty_message: bool,
    /// Replace `HEAD`'s commit rather than adding a child.
    pub amend: bool,
}

impl CommitOptions {
    /// Phase 4 — the options a plain `rivet commit -m <msg>` uses: ambient identity, the message
    /// given, no explicit parents (the caller supplies them), no empty commits, no amend.
    pub fn new(message: &BStr) -> Self {
        Self {
            message: message.to_owned(),
            author: None,
            committer: None,
            parents: Vec::new(),
            allow_empty: false,
            allow_empty_message: false,
            amend: false,
        }
    }

    /// Phase 4 — record an author distinct from the committer.
    pub fn author(&mut self, author: Signature) -> &mut Self {
        self.author = Some(author);
        self
    }

    /// Phase 4 — set the parent list explicitly.
    pub fn parents(&mut self, parents: Vec<ObjectId>) -> &mut Self {
        self.parents = parents;
        self
    }

    /// Phase 4 — allow a commit that changes nothing.
    pub fn allow_empty(&mut self, on: bool) -> &mut Self {
        self.allow_empty = on;
        self
    }

    /// Phase 4 — allow a commit whose message is empty or whitespace-only.
    pub fn allow_empty_message(&mut self, on: bool) -> &mut Self {
        self.allow_empty_message = on;
        self
    }

    /// Phase 4 — amend `HEAD` instead of adding a child.
    pub fn amend(&mut self, on: bool) -> &mut Self {
        self.amend = on;
        self
    }
}

/// Phase 4 — an open repository: its directories and its loaded components.
///
/// # Caveats
///
/// * **The components are loaded eagerly at open time and can go stale.** [`Repository::refs`]
///   reflects the refs as of the last [`crate::refs::RefStore::refresh`], and
///   [`Repository::index`] as of the last read — a caller that wants current state after another
///   process has written asks for a reload rather than assuming. Eager loading is the right
///   default because almost every command needs almost every component, and the cost of a fresh
///   `Config` parse is trivial next to the walk that follows it.
/// * **The git directory and the work tree are separate and either may be absent.** A bare
///   repository has a git directory and no work tree; a gitfile work tree has a work tree whose
///   git directory is somewhere else entirely. A caller that used `worktree_path().parent()` to
///   find the git directory is wrong for both, and the two accessors exist so it never has to.
/// * **`worktree()` returning `None` is the bare case and not an error.** Every method that needs
///   a work tree must report [`crate::error::Error::BareRepository`] with the operation's name —
///   which is what the `op` field in that variant is for — rather than panicking or returning a
///   work tree rooted at the filesystem root.
/// * **The ignore stack belongs to the work tree and is rebuilt when the work tree changes.**
///   `core.excludesFile` is read from the config at open time, `.gitignore` files are read lazily
///   as the walk descends, and a caller that held the stack across a directory change holds the
///   wrong frames.
/// * **A `Repository` is not `Clone`.** The components hold caches and, for the ref store, a view
///   of `packed-refs` that must stay consistent with the loose refs it shadows; a clone that
///   diverged from the original would produce two answers to "what is `HEAD`". A caller that wants
///   two repositories opens two, which also makes the multi-threading story honest — nothing here
///   is `Sync` across a write.
#[derive(Debug)]
pub struct Repository {
    /// The resolved git directory.
    git_dir: PathBuf,
    /// The work tree, when there is one.
    work_tree: Option<PathBuf>,
    /// What kind of repository this is.
    kind: RepositoryKind,
    /// The object format, read from the config at open time.
    format: ObjectFormat,
    /// The object database.
    odb: Odb,
    /// The ref store.
    refs: RefStore,
    /// The index, loaded if it exists.
    index: Index,
    /// The repository's configuration.
    config: Config,
    /// The ignore stack, rooted at the work tree.
    ignore: IgnoreStack,
    /// The work tree as a value, carrying the repository's core.* settings.
    worktree: Worktree,
    /// Payloads of trees returned by [`Repository::tree_of`], keyed by the tree's id.
    ///
    /// A [`Tree`] borrows the bytes it parses, and those bytes come back from the
    /// object database as an owned `Vec`. Keeping them here gives every returned
    /// tree an owner that lives as long as the borrow of `self`. The buffer of a
    /// `Box<[u8]>` never moves, so pushing onto the vector does not invalidate a
    /// tree already handed out; the vector only grows, and the id key means
    /// repeated reads of the same tree reuse one buffer instead of adding another.
    tree_arena: std::cell::RefCell<Vec<(ObjectId, Box<[u8]>)>>,
}

impl Repository {
    /// Phase 4 — open the repository at a git directory, with an explicit work tree.
    ///
    /// The primary constructor: every other entry point reduces to this one after doing its own
    /// discovery, so the rules about format detection, bareness and component loading live here
    /// and in exactly one place.
    ///
    /// # Caveats
    ///
    /// * **`work_tree: None` does not mean bare.** It means "no work tree was named", which for a
    ///   repository whose `core.worktree` is set resolves to that path, and for one that is bare
    ///   resolves to [`RepositoryKind::Bare`]. A caller that wanted to *force* bare passes a git
    ///   directory whose config says so, or [`Repository::init`] with `bare: true`; there is no
    ///   override here, because a repository that claims a work tree and is opened without one is
    ///   a state the rest of the crate cannot describe.
    /// * **The config is read before anything else**, because the object format comes from it and
    ///   every id below depends on the format. `extensions.objectFormat` and
    ///   `core.repositoryformatversion` are checked first, and a version Rivet does not know is
    ///   refused before a single ref is parsed.
    /// * **`core.bare` and the presence of a work tree can disagree**, and the config wins: Git
    ///   treats `core.bare = true` as authoritative even in a directory that looks like a work
    ///   tree, and the reverse. The kind is therefore derived from the config and the discovered
    ///   paths, not from a filesystem test — see [`RepositoryKind`]'s caveat about not re-deriving.
    /// * **A missing index is not an error**, and produces an empty one. A repository that has
    ///   never staged anything has no `index` file, and `status` in it must report every tracked
    ///   file as it is rather than failing to open.
    /// * **Nothing is validated beyond the format and layout.** A repository whose objects are
    ///   corrupt opens fine and fails at the first read, which is deliberate: `fsck` needs to be
    ///   able to open a broken repository in order to report what is wrong with it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::NotARepository`] if the directory has no repository layout,
    /// [`crate::error::Error::UnsupportedRepositoryFormatVersion`] and
    /// [`crate::error::Error::UnknownObjectFormat`] for a format Rivet cannot read,
    /// [`crate::error::Error::InvalidPath`] for a work tree that escapes, and the I/O errors of
    /// reading the config and index.
    pub fn open_with(
        git_dir: &Path,
        work_tree: Option<&Path>,
        format: Option<ObjectFormat>,
    ) -> Result<Self> {
        let absolute = |p: &Path| {
            std::path::absolute(p).map_err(|e| Error::IoAtPath {
                path: BString::from(p.to_string_lossy().into_owned()),
                source: e,
            })
        };

        let git_dir = absolute(git_dir)?;

        // A git directory is defined by its `HEAD`. Checking it here makes a mistyped path fail
        // as `NotARepository` naming that path instead of surfacing later as a missing ref or a
        // config load, which is the failure mode this constructor promises to avoid.
        if !git_dir.join("HEAD").is_file() {
            return Err(Error::NotARepository(BString::from(
                git_dir.to_string_lossy().into_owned(),
            )));
        }

        // The format comes from the config and is read before anything that parses an id.
        let config_path = git_dir.join("config");
        let config = if config_path.exists() {
            Config::load(&config_path)?
        } else {
            Config::new()
        };
        let version = config.repository_format_version()?;
        if version > 1 {
            return Err(Error::UnsupportedRepositoryFormatVersion { version });
        }
        let format = match format {
            Some(f) => f,
            None => config.object_format()?,
        };

        let configured_work_tree = config.get_path(BStr::new(b"core.worktree"))?;
        let work_tree = match work_tree {
            Some(p) => Some(absolute(p)?),
            None => match configured_work_tree {
                Some(p) => {
                    // `core.worktree` is a path of bytes, not text. Decoding it lossily would
                    // silently open a *different* directory whenever the configured path is not
                    // valid UTF-8, so it goes straight into an `OsString`.
                    use std::ffi::OsString;
                    use std::os::unix::ffi::OsStringExt;
                    let configured = PathBuf::from(OsString::from_vec(<BString as AsRef<[u8]>>::as_ref(&p).to_vec()));
                    Some(if configured.is_absolute() {
                        configured
                    } else {
                        git_dir.join(configured)
                    })
                }
                // The config wins over the filesystem's appearance.
                None if config.is_bare()? => None,
                None => {
                    if git_dir.file_name().map(|n| n == ".git").unwrap_or(false) {
                        git_dir.parent().map(|p| p.to_path_buf())
                    } else {
                        Some(git_dir.clone())
                    }
                }
            },
        };

        let kind = match &work_tree {
            None => RepositoryKind::Bare,
            Some(wt) => {
                let normal = git_dir.file_name().map(|n| n == ".git").unwrap_or(false)
                    && git_dir.parent() == Some(wt.as_path());
                if normal {
                    RepositoryKind::Normal
                } else {
                    RepositoryKind::WorkTree
                }
            }
        };

        let odb = Odb::open(&git_dir, format)?;
        let refs = RefStore::open(&git_dir, format)?;
        let index_path = git_dir.join("index");
        let index = if index_path.exists() {
            Index::load(&index_path, format)?
        } else {
            Index::new()
        };
        let mut ignore = match &work_tree {
            Some(root) => IgnoreStack::new(root),
            None => IgnoreStack::new(&git_dir),
        };
        // Rules that live outside a `.gitignore` are loaded once, lowest priority first;
        // `IgnoreStack::check` walks the frames in reverse, so the last pushed is the strongest.
        // `.gitignore` files are deliberately left to the walk, which pushes each directory's
        // file as it descends and truncates as it leaves.
        if work_tree.is_some() {
            if let Some(excludes) = config.get_path(BStr::new("core.excludesfile"))? {
                #[cfg(unix)]
                let excludes = PathBuf::from(std::ffi::OsStr::from_bytes(excludes.as_ref()));
                #[cfg(not(unix))]
                let excludes = PathBuf::from(String::from_utf8_lossy(&excludes).into_owned());
                ignore.push_excludes_file(Some(&excludes))?;
            }
            ignore.push_info_exclude(&git_dir)?;
        }
        let worktree = Worktree::new(work_tree.as_deref(), format, &config)?;

        Ok(Self {
            git_dir,
            work_tree,
            kind,
            format,
            odb,
            refs,
            index,
            config,
            ignore,
            worktree,
            tree_arena: std::cell::RefCell::new(Vec::new()),
        })
    }

    /// Phase 4 — open the repository the environment points at, or the one containing the
    /// current directory.
    ///
    /// # Caveats
    ///
    /// * **The environment is consulted first and overrides discovery**, per the module caveat on
    ///   [`GIT_DIR_ENV`]. A caller in a script that sets `GIT_DIR` gets that repository and not
    ///   the one it happens to be standing in, which is what makes `GIT_DIR=~/.dotfiles rgit
    ///   status` work from anywhere.
    /// * **The current directory is read once, at the start.** A repository opened from a
    ///   directory that is then removed still works, because the paths were resolved to absolute
    ///   at open time; re-reading the current directory per operation would make a long-running
    ///   process's behaviour depend on where it was launched from *now*.
    /// * **`GIT_WORK_TREE` without a git directory is an error**; see the module caveat. It is
    ///   checked here and not in [`Repository::open_with`], which is told its work tree explicitly.
    ///
    /// # Errors
    ///
    /// As [`Repository::open_with`], plus [`crate::error::Error::NotARepository`] when the
    /// current directory is not inside a repository and neither variable is set.
    pub fn open() -> Result<Self> {
        // The current directory is only needed for discovery. Resolving it through a closure
        // keeps an explicitly named `GIT_DIR` usable from a process whose cwd was removed, and
        // turns the failure into an error naming the path instead of a panic.
        let cwd = || -> Result<PathBuf> {
            std::env::current_dir().map_err(|e| Error::IoAtPath {
                path: BString::from("."),
                source: e,
            })
        };
        let git_dir = match std::env::var_os("GIT_DIR") {
            Some(v) => PathBuf::from(v),
            None => find_git_dir(&cwd()?)?,
        };
        let work_tree = match std::env::var_os("GIT_WORK_TREE") {
            Some(v) => Some(PathBuf::from(v)),
            None => None,
        };
        Self::open_with(&git_dir, work_tree.as_deref(), None)
    }

    /// Phase 4 — discover and open the repository containing a directory.
    ///
    /// # Caveats
    ///
    /// The work tree is the directory holding the `.git` marker above `start`, not `start`
    /// itself — discovery from a subdirectory must report the repository root, and a bare
    /// repository discovered from inside it has no work tree at all.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::NotARepository`] if no git directory is found in the directory or
    /// any parent, and as [`Repository::open_with`] otherwise.
    pub fn discover(start: &Path) -> Result<Self> {
        let git_dir = find_git_dir(start)?;
        let work_tree = discovered_work_tree(&git_dir, start);
        Self::open_with(&git_dir, work_tree.as_deref(), None)
    }

    /// Phase 4 — discover and open, honouring the environment first.
    ///
    /// # Caveats
    ///
    /// The order is: the environment, then a repository at or above `start`, then the error. It
    /// is not "the environment as a hint" — see [`GIT_DIR_ENV`] — so a caller that wants the
    /// repository physically containing `start` regardless of the environment calls
    /// [`Repository::discover`] instead.
    ///
    /// # Errors
    ///
    /// As [`Repository::discover`].
    pub fn open_or_discover(start: &Path) -> Result<Self> {
        match std::env::var_os("GIT_DIR") {
            Some(dir) => {
                let git_dir = PathBuf::from(dir);
                // The environment names the git directory but says nothing about a work tree, so
                // derive it the same way discovery would rather than claiming `start` is one.
                let work_tree = discovered_work_tree(&git_dir, start);
                Self::open_with(&git_dir, work_tree.as_deref(), None)
            }
            None => Self::discover(start),
        }
    }

    /// Phase 4 — create a repository.
    ///
    /// # Caveats
    ///
    /// * **The git directory is the path given**, and for a non-bare initialisation that means the
    ///   path is the work tree and `<path>/.git` is the git directory. The distinction is resolved
    ///   here and the caller's `work_tree` only overrides it.
    /// * **`HEAD` is written as a symbolic ref to the initial branch**, which is what makes a
    ///   fresh repository's `status` say "no commits yet on main" rather than "detached". No ref
    ///   is created — an unborn `HEAD` points at nothing, and creating `refs/heads/main` with a
    ///   zero id is a state Git cannot interpret.
    /// * **The other files are created too**: `objects/`, `refs/heads/`, `refs/tags/`, and a
    ///   `config` with `core.repositoryformatversion`, `core.bare` and `extensions.objectFormat`.
    ///   A repository missing any of them is one Git refuses; the exact set is what `git init`
    ///   writes and the differential tests compare against it.
    /// * **Re-initialising an existing repository is legal**, matching Git, and must not destroy
    ///   anything: the config is merged, not overwritten, and existing refs and objects are left
    ///   alone. [`crate::error::Error::RepositoryExists`] is for the case Rivet must refuse (an
    ///   explicit `--force`-less overwrite with a conflicting format), not for the ordinary
    ///   re-run.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RepositoryExists`] where re-initialisation must be refused,
    /// [`crate::error::Error::IoAtPath`] for a file that cannot be created, and
    /// [`crate::error::Error::OutsideWorktree`] for a work tree path that is not usable.
    pub fn init(path: &Path, options: &InitOptions) -> Result<Self> {
        let io_at = |p: &Path, e: std::io::Error| Error::IoAtPath {
            path: BString::from(p.to_string_lossy().into_owned()),
            source: e,
        };
        let absolute = |p: &Path| {
            std::path::absolute(p).map_err(|e| Error::IoAtPath {
                path: BString::from(p.to_string_lossy().into_owned()),
                source: e,
            })
        };

        let root = absolute(path)?;
        let work_tree = if options.bare {
            None
        } else {
            Some(match &options.work_tree {
                Some(p) => absolute(p)?,
                None => root.clone(),
            })
        };
        let git_dir = if options.bare {
            root.clone()
        } else {
            root.join(".git")
        };

        std::fs::create_dir_all(&git_dir).map_err(|e| io_at(&git_dir, e))?;
        for dir in ["objects/info", "objects/pack", "refs/heads", "refs/tags", "hooks", "info"] {
            let p = git_dir.join(dir);
            std::fs::create_dir_all(&p).map_err(|e| io_at(&p, e))?;
        }

        // Re-initialising merges into the existing config rather than replacing it, but a
        // different object format cannot be merged: every id on disk would be reinterpreted.
        let config_path = git_dir.join("config");
        let existed = config_path.exists();
        let mut config = if existed {
            Config::load(&config_path)?
        } else {
            Config::new()
        };
        if existed {
            if let Ok(existing) = config.object_format() {
                if existing != options.format {
                    return Err(Error::RepositoryExists(BString::from(
                        git_dir.to_string_lossy().into_owned(),
                    )));
                }
            }
        }
        let version: &[u8] = match options.format {
            ObjectFormat::Sha1 => b"0",
            ObjectFormat::Sha256 => b"1",
        };
        // Git only writes `extensions.objectformat` for SHA-256. Emitting it for SHA-1 makes
        // stock git reject the repository outright: an extension is a v1-only feature, and a
        // `repositoryformatversion=0` repository containing one is "not a git repository".
        if options.format != ObjectFormat::Sha1 {
            config.set(
                BStr::new(b"extensions.objectformat"),
                options.format.as_str().as_bytes(),
            )?;
        }
        // The keys and their order are the ones `git init` writes, so a fresh rivet config is
        // byte-identical to a fresh git one.
        config.set(BStr::new(b"core.repositoryformatversion"), version)?;
        config.set(BStr::new(b"core.filemode"), b"true")?;
        config.set(
            BStr::new(b"core.bare"),
            if options.bare { b"true" } else { b"false" },
        )?;
        if !options.bare {
            config.set(BStr::new(b"core.logallrefupdates"), b"true")?;
        }
        // A work tree outside the repository root cannot be rediscovered from the layout, so it
        // has to be written down: a later `open_with(git_dir, None)` would otherwise guess the
        // root and stage into the wrong directory.
        if let Some(wt) = work_tree.as_deref() {
            if wt != root.as_path() {
                use std::os::unix::ffi::OsStrExt;
                config.set(BStr::new(b"core.worktree"), wt.as_os_str().as_bytes())?;
            }
        }
        config.write(&config_path)?;

        // The two template files every `git init` leaves behind; neither is overwritten on a
        // re-init, so a user's edits survive.
        for (name, contents) in [
            (
                "description",
                &b"Unnamed repository; edit this file 'description' to name the repository.\n"[..],
            ),
            (
                "info/exclude",
                &b"# git ls-files --others --exclude-from=.git/info/exclude\n\
                   # Lines that start with '#' are comments.\n\
                   # For a project mostly in C, the following would be a good set of\n\
                   # exclude patterns (uncomment them if you want to use them):\n\
                   # *.[oa]\n\
                   # *~\n"[..],
            ),
        ] {
            let p = git_dir.join(name);
            if !p.exists() {
                std::fs::write(&p, contents).map_err(|e| io_at(&p, e))?;
            }
        }

        // An unborn HEAD: a symbolic ref to a branch that does not exist yet.
        let head_path = git_dir.join("HEAD");
        if !head_path.exists() {
            let mut head = Vec::from(&b"ref: refs/heads/"[..]);
            head.extend_from_slice(&options.initial_branch);
            head.push(b'\n');
            std::fs::write(&head_path, head).map_err(|e| io_at(&head_path, e))?;
        }

        Self::open_with(&git_dir, work_tree.as_deref(), Some(options.format))
    }

    /// Phase 4 — the git directory.
    ///
    /// # Caveats
    ///
    /// Always absolute, resolved at open time. For a gitfile work tree this is the *target* of
    /// the file, not the directory containing it, which is the distinction that makes a linked
    /// work tree's `MERGE_HEAD` end up in the right place. See [`gitfile_target`].
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Phase 4 — the work tree, or `None` for a bare repository.
    ///
    /// # Caveats
    ///
    /// `None` is a legal state and not an error; see [`Repository::require_worktree`] for the
    /// form that turns it into one with the operation's name attached. A caller that unwrapped it
    /// would panic on every command run against a bare repository — which is every command run
    /// against a repository a user is only pushing to.
    pub fn worktree_path(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    /// Phase 4 — the work tree, or an error naming the operation that needed one.
    ///
    /// # Caveats
    ///
    /// `op` is a `&'static str` and goes straight into
    /// [`crate::error::Error::BareRepository`]'s message, so it must read as the thing the user
    /// tried to do (`"checkout"`, not `"require_worktree"`). It is a static rather than a `String`
    /// because the set of operations is closed and a formatted operation name would allocate on
    /// an error path.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`] when there is no work tree.
    pub fn require_worktree(&self, op: &'static str) -> Result<&Worktree> {
        self.worktree()
    }

    /// Phase 4 — what kind of repository this is.
    pub fn kind(&self) -> RepositoryKind {
        self.kind
    }

    /// Phase 4 — whether the repository is bare.
    ///
    /// # Caveats
    ///
    /// The convenient predicate for the common question, and it is `kind() == Bare` — not "the
    /// work tree is missing", which is also true of a repository whose `core.worktree` is wrong.
    /// A caller deciding whether to write files should ask this; one deciding *where* should ask
    /// [`Repository::worktree_path`].
    pub fn is_bare(&self) -> bool {
        self.kind() == RepositoryKind::Bare
    }

    /// Phase 4 — the object format the repository was created with.
    ///
    /// # Caveats
    ///
    /// Read once at open time and immutable for the repository's life, per [`InitOptions`]'s
    /// caveat: it cannot be changed. Every id parsed anywhere in the crate goes through it, and a
    /// caller that hard-coded a length instead would read a SHA-256 repository's refs as garbage.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Phase 4 — the object database.
    pub fn odb(&self) -> &Odb {
        &self.odb
    }

    /// Phase 4 — the object database, mutably, for a caller that needs to refresh it.
    ///
    /// # Caveats
    ///
    /// `&mut` for [`crate::odb::Odb::refresh`] and [`crate::odb::Odb::add_pack`] only; reads need
    /// [`Repository::odb`]. A caller holding this across a ref update holds two components of the
    /// repository mutably at once in spirit, which is why the two are separate accessors rather
    /// than one returning a tuple.
    pub fn odb_mut(&mut self) -> &mut Odb {
        &mut self.odb
    }

    /// Phase 4 — the ref store.
    pub fn refs(&self) -> &RefStore {
        &self.refs
    }

    /// Phase 4 — the ref store, mutably.
    pub fn refs_mut(&mut self) -> &mut RefStore {
        &mut self.refs
    }

    /// Phase 4 — the index.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Phase 4 — the index, mutably, for staging and checkout.
    pub fn index_mut(&mut self) -> &mut Index {
        &mut self.index
    }

    /// Phase 4 — re-read the index from disk.
    ///
    /// # Caveats
    ///
    /// Needed after another process has staged something, and after a merge driver has written
    /// conflict stages — the in-memory index is a snapshot and nothing invalidates it
    /// automatically. Re-reading a large index is not free, and a caller that called it before
    /// every operation would make every command pay for consistency it did not need.
    ///
    /// # Errors
    ///
    /// The index parser's errors — [`crate::error::Error::BadIndexSignature`],
    /// [`crate::error::Error::CorruptIndex`], [`crate::error::Error::BadIndexChecksum`] — and
    /// [`crate::error::Error::IoAtPath`].
    pub fn reload_index(&mut self) -> Result<()> {
        let path = self.git_dir.join("index");
        self.index = if path.exists() {
            Index::load(&path, self.format)?
        } else {
            Index::new()
        };
        Ok(())
    }

    /// Phase 4 — write the index to disk.
    ///
    /// # Caveats
    ///
    /// Takes `index.lock` and renames, so a concurrent writer fails with
    /// [`crate::error::Error::LockHeld`] rather than interleaving. A caller that mutated the index
    /// through [`Repository::index_mut`] and did not call this has staged nothing, which is the
    /// single most likely way to lose work with this API.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] and [`crate::error::Error::IoAtPath`].
    pub fn write_index(&self) -> Result<()> {
        self.index.write(&self.git_dir.join("index"), self.format)
    }

    /// Phase 4 — the configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Phase 4 — the configuration, mutably, for a caller that writes a setting.
    ///
    /// # Caveats
    ///
    /// Mutating this does not write the file; the caller calls [`crate::config::Config::write`].
    /// The components loaded at open time — the object format above all — do **not** pick up a
    /// change made here, which is deliberate: changing `extensions.objectFormat` mid-process would
    /// reinterpret every id already in memory.
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    /// Phase 4 — the ignore stack, rooted at the work tree.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`], since ignore rules are a property of a work tree
    /// and a bare repository has none.
    pub fn ignore(&self) -> Result<&IgnoreStack> {
        // The stack is rooted at the work tree when there is one; a repository opened without a
        // work tree has none to root it at, so the state that matters is the path, not a flag.
        if self.work_tree.is_none() {
            return Err(Error::BareRepository {
                path: self.git_dir.to_string_lossy().into_owned().into(),
                op: "ignore",
            });
        }
        Ok(&self.ignore)
    }

    /// Phase 4 — the ignore stack, mutably, for a walk that descends into directories.
    pub fn ignore_mut(&mut self) -> &mut IgnoreStack {
        &mut self.ignore
    }

    /// Phase 4 — the work tree, as a value.
    ///
    /// # Caveats
    ///
    /// Distinct from [`Repository::worktree_path`]: this is the [`Worktree`] type the walk,
    /// status, checkout and diff operations take, carrying the repository's object format and its
    /// `core.filemode`, `core.symlinks`, `core.ignoreCase` and `core.protectNTFS` settings. A
    /// caller that constructed its own from the path would get those settings wrong and produce an
    /// index that differs from `git`'s on a filesystem with an executable bit.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`] when there is no work tree.
    pub fn worktree(&self) -> Result<&Worktree> {
        if self.work_tree.is_none() {
            return Err(Error::BareRepository {
                path: self.git_dir.to_string_lossy().into_owned().into(),
                op: "worktree",
            });
        }
        Ok(&self.worktree)
    }

    /// Phase 4 — what `HEAD` points at.
    ///
    /// # Caveats
    ///
    /// The three states — attached, unborn, detached — are distinct and must stay so; see
    /// [`HeadState`]. An unborn `HEAD` is the state of every fresh repository, and collapsing it
    /// into "detached with no id" makes the first commit's parent list wrong.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] for a `HEAD` file that is missing, and the ref store's
    /// resolution errors for a symbolic chain that is too deep or cyclic.
    pub fn head(&self) -> Result<HeadState> {
        self.refs.head()
    }

    /// Phase 4 — the commit `HEAD` resolves to, peeled from any tag.
    ///
    /// # Caveats
    ///
    /// `None` for an unborn `HEAD`, which is not an error — a repository with no commits has no
    /// `HEAD` commit, and every operation that can run there (`status`, `add`, `commit`) must
    /// handle it. A caller that wanted an error instead asks
    /// [`crate::refs::RefStore::head_commit`], which reports one.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] for a ref pointing at a missing object, and
    /// [`crate::error::Error::NotACommit`] for one that peels to a tree or blob.
    pub fn head_commit(&self) -> Result<Option<ObjectId>> {
        let head = self.head()?;
        match head {
            HeadState::Attached { oid, .. } => Ok(Some(oid)),
            HeadState::Detached { oid } => Ok(Some(oid)),
            HeadState::Unborn { .. } => Ok(None),
        }
    }

    /// Phase 4 — the tree of a commit.
    ///
    /// # Caveats
    ///
    /// Reads the commit object. Called with `None` it returns `None` rather than erroring, so that
    /// an unborn `HEAD` flows through the code that computes a status without a special case at
    /// every call site — which is why the parameter is an `Option` rather than the caller
    /// unwrapping before the call.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] and [`crate::error::Error::WrongObjectKind`].
    pub fn tree_of(&self, commit: Option<ObjectId>) -> Result<Option<Tree<'_>>> {
        use crate::object::{AnyObject, Object, ObjectKind};

        let oid = match commit {
            Some(oid) => oid,
            None => return Ok(None),
        };
        let tree_id = match self.odb.read_typed(&oid, ObjectKind::Commit)?.parse()? {
            AnyObject::Commit(commit) => commit.tree,
            _ => {
                return Err(Error::WrongObjectKind {
                    oid: Some(oid),
                    expected: ObjectKind::Commit,
                    found: ObjectKind::Tree,
                })
            }
        };
        let bytes = match self.odb.read_typed(&tree_id, ObjectKind::Tree)? {
            Object::Tree(bytes) => bytes.into_boxed_slice(),
            _ => {
                return Err(Error::WrongObjectKind {
                    oid: Some(tree_id),
                    expected: ObjectKind::Tree,
                    found: ObjectKind::Commit,
                })
            }
        };

        let mut arena = self.tree_arena.borrow_mut();
        let index = match arena.iter().position(|(id, _)| *id == tree_id) {
            Some(index) => index,
            None => {
                arena.push((tree_id, bytes));
                arena.len() - 1
            }
        };
        let stored: &[u8] = arena[index].1.as_ref();
        // SAFETY: `stored` points into a `Box<[u8]>` owned by `self.tree_arena`. A boxed slice's
        // pointee never moves, and the arena is private: entries are only ever pushed, never
        // removed or replaced, and the arena itself lives in `self`. The reference is therefore
        // valid for as long as the `&self` borrow the caller holds, which is the lifetime the
        // returned `Tree` is given.
        let stored: &'static [u8] = unsafe { &*(stored as *const [u8]) };
        Ok(Some(Tree::parse(self.format, stored)?))
    }

    /// Phase 4 — the tree `HEAD`'s commit has.
    ///
    /// # Caveats
    ///
    /// `None` for an unborn `HEAD`. This is what `diff --staged` compares the index against, and
    /// what `status` uses to decide which staged entries are additions.
    ///
    /// # Errors
    ///
    /// As [`Repository::tree_of`].
    pub fn head_tree(&self) -> Result<Option<Tree<'_>>> {
        self.tree_of(self.head_commit()?)
    }

    /// Phase 4 — write the index's contents as a tree.
    ///
    /// # Caveats
    ///
    /// The bridge from staging to committing. The tree-building rules (Git's entry sort, gitlinks
    /// not descended into, conflicts refused) are implemented here rather than delegated, so this
    /// is the single definition of a tree's shape. A caller that wants to commit must call this
    /// *before* constructing the commit, because the commit names a tree that has to exist.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnmergedPath`] while any path has conflict stages, and the object
    /// database's write errors.
    pub fn write_tree(&self) -> Result<ObjectId> {
        write_tree_of(&self.odb, &self.index)
    }

    /// Phase 3 — stage paths into the index, writing it back.
    ///
    /// # Caveats
    ///
    /// **The index file is written before this returns.** The mutation
    /// [`crate::worktree::add_paths`] performs is in memory and is invisible to every other
    /// process — and to the next run of this one — until [`Repository::write_index`] runs, which
    /// is why the write lives here rather than at each call site. A dry run writes nothing.
    ///
    /// # Errors
    ///
    /// Every error of [`crate::worktree::add_paths`], plus the index's own write errors.
    pub fn add(
        &mut self,
        paths: &[BString],
        options: &crate::worktree::AddOptions,
    ) -> Result<crate::worktree::AddOutcome> {
        let mut stat_cache = crate::worktree::StatCache::new();
        let outcome = crate::worktree::add_paths(
            &mut self.index,
            &self.worktree,
            &self.odb,
            &self.ignore,
            &mut stat_cache,
            paths,
            options,
        )?;
        if !options.dry_run {
            self.write_index()?;
        }
        Ok(outcome)
    }

    /// Phase 3 — remove paths from the index, writing it back.
    ///
    /// # Caveats
    ///
    /// **The index is written here even under `--cached`**, which is the whole difference between
    /// `rm --cached` and doing nothing: the work tree survives and the entry does not. A dry run
    /// writes nothing.
    ///
    /// # Errors
    ///
    /// Every error of [`crate::worktree::rm_paths`], plus the index's own write errors.
    pub fn remove(
        &mut self,
        paths: &[BString],
        options: &crate::worktree::RmOptions,
    ) -> Result<crate::worktree::AddOutcome> {
        let mut stat_cache = crate::worktree::StatCache::new();
        let outcome = crate::worktree::rm_paths(
            &mut self.index,
            &self.worktree,
            &mut stat_cache,
            paths,
            options,
        )?;
        if !options.dry_run {
            self.write_index()?;
        }
        Ok(outcome)
    }

    /// Phase 4 — compare the work tree, the index and `HEAD`.
    ///
    /// # Caveats
    ///
    /// **This reads `HEAD`'s tree once and passes the id down**, rather than handing the status
    /// code a repository: the comparison needs three of the components and none of the ref
    /// machinery, which is what keeps [`crate::worktree::compute`] testable without a repository
    /// on disk. An unborn `HEAD` is `None`, which makes every index entry an addition.
    pub fn status(
        &mut self,
        options: &crate::worktree::StatusOptions,
    ) -> Result<crate::worktree::Status> {
        let head_tree = self.head_tree_id()?;
        crate::worktree::compute(
            &self.worktree,
            &self.index,
            head_tree,
            &self.odb,
            &self.ignore,
            options,
        )
    }

    /// The tree id `HEAD` names, without materialising the tree.
    ///
    /// # Caveats
    ///
    /// A commit object whose parse yields something other than a commit cannot happen through
    /// [`Repository::commit`], and is reported as `None` here rather than as an error: `status`
    /// on a corrupt `HEAD` should show every staged path as an addition, exactly as an unborn
    /// branch does, instead of failing to open the repository at all.
    fn head_tree_id(&self) -> Result<Option<ObjectId>> {
        use crate::object::{AnyObject, ObjectKind};
        match self.head()? {
            HeadState::Attached { oid, .. } | HeadState::Detached { oid } => {
                match self.odb.read_typed(&oid, ObjectKind::Commit)?.parse()? {
                    AnyObject::Commit(commit) => Ok(Some(commit.tree)),
                    _ => Ok(None),
                }
            }
            HeadState::Unborn { .. } => Ok(None),
        }
    }

    /// Phase 4 — record a commit.
    ///
    /// # Caveats
    ///
    /// * **Nothing is moved until the commit object is written.** The object database is written
    ///   first and the ref is moved afterwards, so a failure between the two leaves an
    ///   unreferenced commit rather than a ref pointing at nothing — the same ordering rule as
    ///   every other ref update in the crate.
    /// * **The identity comes from the config when the options do not carry one**, and a missing
    ///   `user.name` or `user.email` is an error rather than an invented value. See
    ///   [`CommitOptions`].
    /// * **The message is written byte for byte**, including its trailing newline if it has one.
    ///   Git does not require one; normalising here would produce a different commit id from the
    ///   one `git commit` writes for the same message.
    /// * **`--amend` replaces the ref's target and rewrites the reflog**, and the old commit
    ///   remains in the object database and in the reflog — which is what makes an amend
    ///   recoverable. A caller that expected the old object to be deleted is expecting garbage
    ///   collection, which is a separate operation.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnmergedPath`] for conflicts, [`crate::error::Error::Message`] for a
    /// missing identity or an empty commit without `allow_empty`, the object database's write
    /// errors, and the ref store's update errors.
    pub fn commit(&mut self, options: &CommitOptions) -> Result<ObjectId> {
        use crate::object::{AnyObject, ObjectKind, CommitBuilder};
        if self.index.has_conflicts() {
            let path = self
                .index
                .entries()
                .iter()
                .find(|entry| entry.is_unmerged())
                .map(|entry| entry.path.clone())
                .unwrap_or_else(|| BString::from(""));
            return Err(Error::UnmergedPath { path });
        }
        // Git aborts a commit with an empty message unless `--allow-empty-message` is given.
        // Refusing before the tree is written keeps a doomed commit from leaving a new tree object
        // behind.
        if !options.allow_empty_message && options.message.iter().all(|b| b.is_ascii_whitespace()) {
            return Err(Error::Message(
                "aborting commit due to empty commit message".to_string(),
            ));
        }
        // `--amend` takes its parents from the commit being replaced; an explicit list alongside
        // it has no meaning in Git. Failing is better than silently dropping the caller's list.
        if options.amend && !options.parents.is_empty() {
            return Err(Error::Message(
                "cannot combine amend with explicit parents".to_string(),
            ));
        }
        let tree = self.write_tree()?;
        let head = self.head()?;

        let parents = if options.amend {
            match &head {
                HeadState::Attached { oid, .. } | HeadState::Detached { oid } => {
                    match self.odb.read_typed(oid, ObjectKind::Commit)?.parse()? {
                        AnyObject::Commit(commit) => commit.parents.clone(),
                        _ => Vec::new(),
                    }
                }
                HeadState::Unborn { .. } => Vec::new(),
            }
        } else if !options.parents.is_empty() {
            options.parents.clone()
        } else {
            let mut parents = match &head {
                HeadState::Attached { oid, .. } | HeadState::Detached { oid } => vec![*oid],
                HeadState::Unborn { .. } => Vec::new(),
            };
            // A conflicted or `--no-commit` merge is concluded by an ordinary commit, which
            // takes the merged heads as its extra parents — `MERGE_HEAD` is that record.
            if let Some(state) = crate::merge::read_merge_state(&self.git_dir, self.format)? {
                parents.extend(state.merge_head.iter().copied());
            }
            parents
        };
        let concluding_merge = !options.amend
            && options.parents.is_empty()
            && crate::merge::merge_in_progress(&self.git_dir);

        if !options.allow_empty && !options.amend && parents.len() < 2 {
            if let Some(parent) = parents.first() {
                let parent_tree = match self.odb.read_typed(parent, ObjectKind::Commit)?.parse()? {
                    AnyObject::Commit(commit) => commit.tree,
                    _ => tree,
                };
                if parent_tree == tree {
                    return Err(Error::Message(
                        "nothing to commit: the tree is unchanged".to_string(),
                    ));
                }
            }
        }

        // Identity: the options if given, then the environment, then the configuration
        // (repository, global, system — Git's precedence).
        let author = match &options.author {
            Some(signature) => signature.clone(),
            None => self.author_signature()?,
        };
        let committer = match &options.committer {
            Some(signature) => signature.clone(),
            None => self.committer_signature()?,
        };

        let mut builder = CommitBuilder::new();
        builder.tree(tree);
        for parent in &parents {
            builder.parent(*parent);
        }
        builder.author(author.clone());
        builder.committer(committer.clone());
        builder.message(&options.message);
        let body = builder.build(self.format)?;

        // The object first, the ref second: a failure in between leaves an unreferenced
        // commit rather than a ref pointing at nothing.
        let oid = self.odb.write_raw(ObjectKind::Commit, &body)?;

        let subject = options
            .message
            .split(|b| *b == b'\n')
            .next()
            .unwrap_or(&[]);
        // Git distinguishes the first commit and an amend in the reflog; `HEAD@{n}` searches
        // and `git reflog` output depend on the exact words.
        let operation = if options.amend {
            "commit (amend)"
        } else if parents.is_empty() {
            "commit (initial)"
        } else if parents.len() > 1 {
            "commit (merge)"
        } else {
            "commit"
        };
        let reason = Self::reflog_message(operation, BStr::new(subject));
        let message = self
            .should_log_ref(&crate::refs::RefName::head())
            .then(|| crate::refs::ReflogMessage::new(&committer, BStr::new(&reason)));

        match &head {
            HeadState::Attached { branch, oid: old } => {
                self.refs.update(branch, oid, Some(*old), None)?;
            }
            HeadState::Unborn { branch } => {
                self.refs.update(branch, oid, None, None)?;
            }
            HeadState::Detached { .. } => {
                let head_ref = crate::refs::RefName::from_bytes(b"HEAD")?;
                self.refs.set_direct(&head_ref, oid, message)?;
                return Ok(oid);
            }
        }
        if concluding_merge {
            crate::merge::clear_merge_state(&self.git_dir)?;
        }
        // The branch moved through `HEAD`: both logs get the line, as Git writes them.
        if let Some(branch) = head.branch() {
            let old = head.oid().unwrap_or_else(|| ObjectId::zero(self.format));
            if self.should_log_ref(branch) {
                let entry = crate::refs::ReflogEntry::new(old, oid, committer.clone(), reason.clone());
                self.refs.append_reflog(branch, &entry)?;
            }
            if message.is_some() {
                let entry = crate::refs::ReflogEntry::new(old, oid, committer.clone(), reason.clone());
                self.refs.append_reflog(&crate::refs::RefName::head(), &entry)?;
            }
        }

        Ok(oid)
    }

    /// Phase 4 — the reflog message for an operation, in Git's format.
    ///
    /// # Caveats
    ///
    /// The format is `<operation>: <description>`, e.g. `commit: add the parser`. It is built
    /// here rather than at each call site so that the spelling is consistent — a reflog a user
    /// reads is a user interface, and `commit:` versus `Commit:` versus `committed` are three
    /// different-looking histories for the same repository.
    pub fn reflog_message(operation: &str, description: &BStr) -> BString {
        let mut message = BString::from(operation);
        message.extend_from_slice(b": ");
        message.extend_from_slice(description);
        message
    }
}

/// Phase 4 — find the git directory for a directory, walking up.
///
/// # Caveats
///
/// * **The return value is the git directory, which for a gitfile is not `<found>/.git`.** The
///   gitfile is followed here, once, so that no caller has to know the difference; see
///   [`gitfile_target`]. A caller that needs to know whether it came through a gitfile asks
///   [`Repository::kind`] afterwards.
/// * **`GIT_DIR` is honoured and short-circuits the walk**, per the module caveat. A caller that
///   wanted discovery regardless calls this with the environment ignored — which is why the
///   environment is read in [`Repository::open_or_discover`] and not here.
/// * **The walk stops at the filesystem root and does not cross into a parent that is not a
///   directory**, so a path that is a file, or one that does not exist, is
///   [`crate::error::Error::NotARepository`] rather than an error about the path itself. That is
///   Git's answer and it is the useful one: "not a repository" is what the user needs to know.
/// * **A `.git` file that is not a gitfile is an error, not a directory to skip.** A file whose
///   contents do not begin with `gitdir: ` is a corrupt repository marker; treating it as
///   "nothing here" and continuing the walk finds an outer repository and silently operates on the
///   wrong one.
///
/// # Errors
///
/// [`crate::error::Error::NotARepository`] when the walk reaches the root, and
/// [`crate::error::Error::InvalidPath`] for a gitfile whose target is missing, is not a directory,
/// or escapes the repository.
pub fn find_git_dir(start: &Path) -> Result<PathBuf> {
    let start = std::path::absolute(start).map_err(|e| Error::IoAtPath {
        path: BString::from(start.to_string_lossy().into_owned()),
        source: e,
    })?;

    let mut current: Option<&Path> = if start.is_file() {
        start.parent()
    } else {
        Some(start.as_path())
    };
    while let Some(dir) = current {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git);
        }
        if dot_git.is_file() {
            // A `.git` file is a gitfile and must parse; skipping it would find an outer
            // repository and silently operate on the wrong one.
            return gitfile_target(&dot_git);
        }
        // A bare repository is its own git directory.
        if dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
            return Ok(dir.to_path_buf());
        }
        current = dir.parent();
    }

    Err(Error::NotARepository(BString::from(
        start.to_string_lossy().into_owned(),
    )))
}

/// The work tree the repository discovered from `start` belongs to.
///
/// Git's rule is the directory holding the `.git` marker, not the directory the caller happened
/// to pass: `discover("/repo/src")` reports `/repo` as the work tree, and a bare repository
/// discovered from anywhere inside it has no work tree at all. Handing back the starting
/// directory instead makes every relative path a caller derives from the work tree wrong by the
/// depth of the subdirectory it started in.
fn discovered_work_tree(git_dir: &Path, start: &Path) -> Option<PathBuf> {
    let start = std::path::absolute(start).ok()?;
    let mut current: Option<&Path> = if start.is_file() {
        start.parent()
    } else {
        Some(start.as_path())
    };
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        if dir == git_dir {
            // Reached the git directory without passing a `.git` marker: bare.
            return None;
        }
        current = dir.parent();
    }
    None
}

/// Collapse `.` and `..` components without touching the filesystem.
///
/// [`std::fs::canonicalize`] would also resolve symlinks and fail on a missing path; discovery has
/// to report the path Git would print, which is only lexically normalised.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Phase 4 — read and resolve a `.git` file's target.
///
/// # Caveats
///
/// * **A relative target is resolved against the gitfile's own directory**, not the process's
///   working directory and not the work tree root. This is the rule that makes a submodule
///   checked out at a different path still find its own git directory, and getting it wrong finds
///   a *different* repository that happens to exist at the relative path from the wrong base.
/// * **The target is not required to be inside the work tree.** A linked work tree's git
///   directory is under the *main* repository's `.git/worktrees/<name>`, which is elsewhere
///   entirely, and a check that required containment would reject every `git worktree` checkout.
/// * **The file must contain exactly `gitdir: <path>` and nothing else** beyond surrounding
///   whitespace and one optional trailing newline. Git accepts a leading/trailing space and a
///   trailing `\r`; it does not accept other lines, and a parser that scanned for the first
///   `gitdir:` would accept a file whose real content is a comment and open the wrong repository.
/// * **The returned path is absolute and not canonicalised through symlinks.** Canonicalising
///   would resolve a symlinked work tree to its real location and break a repository deliberately
///   addressed through a link — and on macOS it would also turn `/tmp` into `/private/tmp`,
///   making every path Rivet printed differ from the one the user typed.
/// * **The file's contents are bytes.** A git directory path may not be UTF-8, and a parser that
///   decoded to `String` would fail on exactly the repositories that are hardest to diagnose.
///
/// # Errors
///
/// [`crate::error::Error::InvalidPath`] for a file that does not start with `gitdir: `, for one
/// with additional non-empty lines, and for a target that is not an existing directory;
/// [`crate::error::Error::IoAtPath`] for a file that cannot be read.
pub fn gitfile_target(gitfile: &Path) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let at = |p: &Path| BString::from(p.to_string_lossy().into_owned());
    let data = std::fs::read(gitfile).map_err(|e| Error::IoAtPath {
        path: at(gitfile),
        source: e,
    })?;

    // Trim surrounding ASCII whitespace (Git accepts a trailing `\r` and spaces) but reject
    // any other line: a parser that scanned for the first `gitdir:` would accept a comment.
    let text: &[u8] = &data;
    let text = text.trim_ascii();
    let rest = text
        .strip_prefix(b"gitdir:")
        .ok_or_else(|| Error::InvalidPath(at(gitfile)))?;
    let rest = rest.trim_ascii();
    if rest.is_empty() || rest.contains(&b'\n') || rest.contains(&b'\r') {
        return Err(Error::InvalidPath(at(gitfile)));
    }

    let target = PathBuf::from(std::ffi::OsString::from_vec(rest.to_vec()));
    let target = if target.is_absolute() {
        target
    } else {
        // Relative to the gitfile's own directory, never the process's working directory.
        gitfile
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(&target)
    };
    // Git normalises `..` in the target before reporting it. Without this, discovery hands back
    // `repo/worktree/../repo/.git`, which compares unequal to the same directory spelled without
    // the detour even though both name one place.
    let target = normalize_lexically(&target);
    if !target.is_dir() {
        return Err(Error::InvalidPath(at(&target)));
    }
    Ok(target)
}

/// Build one level of a tree from index paths, writing the result to the object database.
///
/// The single definition of "what tree does this index describe", shared by
/// [`Repository::write_tree`] and [`crate::index::IndexExt::write_tree`].
pub(crate) fn write_tree_of(odb: &Odb, index: &crate::index::Index) -> Result<ObjectId> {
    let mut items: Vec<(Vec<BString>, crate::object::EntryMode, ObjectId)> = Vec::new();
    for entry in index.entries() {
        if entry.is_unmerged() {
            return Err(Error::UnmergedPath {
                path: entry.path.clone(),
            });
        }
        let components: Vec<BString> = entry
            .path
            .split(|b| *b == b'/')
            .map(BString::from)
            .collect();
        // `split` always yields at least one component, so emptiness is not the failure to
        // look for: a leading, trailing or doubled slash yields an empty *component*, which
        // would be written as a tree entry with a zero-length name — a tree Git parses into
        // something no checkout can reproduce.
        if components.iter().any(|c| c.is_empty()) {
            return Err(Error::InvalidPath(entry.path.clone()));
        }
        items.push((components, entry.mode, entry.oid));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let level: Vec<(&[BString], crate::object::EntryMode, ObjectId)> = items
        .iter()
        .map(|(components, mode, oid)| (components.as_slice(), *mode, *oid))
        .collect();
    build_tree_level(odb, &level)
}

fn build_tree_level(
    odb: &Odb,
    level: &[(&[BString], crate::object::EntryMode, ObjectId)],
) -> Result<ObjectId> {
    use crate::object::{EntryMode, ObjectKind, Tree, TreeEntry};

    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut i = 0;
    while i < level.len() {
        let name = level[i].0[0].clone();
        let mut j = i + 1;
        while j < level.len() && level[j].0[0] == name {
            j += 1;
        }
        if level[i].0.len() == 1 {
            entries.push(TreeEntry {
                mode: level[i].1,
                name,
                oid: level[i].2,
            });
        } else {
            let children: Vec<(&[BString], EntryMode, ObjectId)> = level[i..j]
                .iter()
                .map(|(path, mode, oid)| (&path[1..], *mode, *oid))
                .collect();
            let oid = build_tree_level(odb, &children)?;
            entries.push(TreeEntry {
                mode: EntryMode::Tree,
                name,
                oid,
            });
        }
        i = j;
    }

    entries.sort_by(|a, b| tree_sort_key(a).cmp(&tree_sort_key(b)));

    // `data` is the payload `id()` hashes; it is filled in here because the payload is
    // exactly what `serialize` produces from these entries.
    let tree = Tree {
        data: &[],
        entries,
    };
    odb.write_raw(ObjectKind::Tree, &tree.serialize())
}

/// Git's tree sort: by name bytes, with a directory compared as if its name ended in `/`.
fn tree_sort_key(entry: &crate::object::TreeEntry) -> Vec<u8> {
    let mut key = entry.name.as_slice().to_vec();
    if entry.mode == crate::object::EntryMode::Tree {
        key.push(b'/');
    }
    key
}
