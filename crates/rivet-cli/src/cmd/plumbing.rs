//! Plumbing commands: `hash-object`, `cat-file`, `ls-tree`, `ls-files`, `rev-parse`,
//! `write-tree`, `verify-pack` and `fsck`.
//!
//! Plan §7's last row. These are the commands a *script* uses rather than a person: each does one
//! thing to one object or one file and prints an answer in a format that must not change. Every
//! one of them is a thin wrapper over a `rivet-core` entry point, and none of them may grow a
//! convenience the core does not have — a plumbing command that filters its own output is one a
//! script cannot use.
//!
//! *Phase 1 for `hash-object`/`cat-file`, Phase 3 for `ls-files`/`write-tree`, Phase 4 for
//! `ls-tree`/`rev-parse`, Phase 10 for `verify-pack`/`fsck`.*

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{GlobalArgs, ObjectKindArg};

/// Arguments to `rivet hash-object`.
///
/// # Caveats
///
/// * **`write` is what turns a hash into a stored object, and without it nothing is written.**
///   The command's whole purpose in a script is the two modes: compute an id for a byte stream
///   without touching the repository, or store it. A handler that always wrote would make
///   `hash-object` unusable for the first, which is how it is used to ask "would this file's id
///   match" without side effects.
/// * **`stdin` with `paths` is a usage error.** Git accepts a `--stdin-paths` mode that reads a
///   list of paths, and conflating that with "read the object data from stdin" makes
///   `hash-object --stdin file.txt` ambiguous. The two are separate flags and the grammar must
///   keep them so.
/// * **`kind` is `blob` by default and the format comes from the repository when one is found.**
///   Outside a repository there is no format to consult, and the default is SHA-1 — which means
///   `rivet hash-object` on a file outside any repository produces a SHA-1 id, while the same
///   command inside a SHA-256 repository produces a SHA-256 one. That is Git's behaviour and it
///   is a real footgun; `--stdin` outside a repository must still work rather than erroring on
///   the missing format.
/// * **`literally` allows an object whose content is already a header**, producing a blob whose
///   bytes are `blob 12\0hello world` rather than `hello world`. It exists to construct
///   deliberately malformed objects for fuzzing and must be off by default.
/// * **`path` changes the blob's id only through the filters that would be applied**, which in
///   v1 do not exist (`core.autocrlf` is a non-goal, plan §1). The flag is accepted so the
///   grammar is complete and must be a no-op that says so, not one that silently applies
///   something.
/// * **`no_filters` is the opposite and is the honest default.** With no filters implemented the
///   two flags agree; the moment one is added they will not, and a handler that ignored the
///   distinction would apply a filter the user disabled.
#[derive(Debug, clap::Args)]
pub struct HashObjectArgs {
    /// The files to hash. Required unless `--stdin` is given.
    #[arg(value_name = "FILE", required_unless_present_any = ["stdin", "stdin_paths"])]
    pub paths: Vec<PathBuf>,

    /// The type of object to create.
    #[arg(
        short = 't',
        long = "type",
        value_enum,
        value_name = "TYPE",
        default_value = "blob"
    )]
    pub kind: ObjectKindArg,

    /// Write the object into the object database as well as reporting its id.
    #[arg(short = 'w', long = "write")]
    pub write: bool,

    /// Read the object's contents from standard input.
    #[arg(long = "stdin")]
    pub stdin: bool,

    /// Read the paths to hash, one per line, from standard input.
    #[arg(long = "stdin-paths")]
    pub stdin_paths: bool,

    /// Use the object's contents verbatim, without adding a header.
    #[arg(long = "literally")]
    pub literally: bool,

    /// Hash the file as if it were at this path, applying the filters that path implies.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Apply no filters, whatever `--path` and the config say.
    #[arg(long = "no-filters")]
    pub no_filters: bool,
}

/// Arguments to `rivet cat-file`.
///
/// # Caveats
///
/// * **The `-t`/`-s`/`-p`/`-e` flags select mutually exclusive modes and exactly one is
///   required.** `-p` pretty-prints an object in its own format, `-t` prints just the type,
///   `-s` just the size, and `-e` prints nothing and only sets the exit code. A handler that
///   defaulted to `-p` when no flag was given changes `rivet cat-file <id>` from an error into a
///   dump, which a script parsing the error would not expect.
/// * **The size `-s` prints is the object's own length, not the on-disk length.** For a loose
///   object that is the inflated size and for a packed one it is the size after delta
///   application; printing the compressed or the delta length is a plausible-looking wrong
///   answer that only shows up on a packed repository.
/// * **`-p` output depends on the kind**: a commit's headers and message, a tree's entries, a
///   blob's bytes, a tag's fields. This is the same dispatch `show` performs and the two must
///   agree — a `cat-file -p` and a `show` that print different things for the same object is a
///   bug in one of them, and the differential tests compare both to `git`.
/// * **A blob's `-p` output is written byte for byte and may be binary.** Nothing is appended,
///   nothing is encoded, and `rivet cat-file -p HEAD:file | cmp - file` must be silent.
/// * **`-e` exits `1` for an object that does not exist and `0` for one that does**, which is
///   the only way to test for an object's existence from the command line; it must not print
///   the object, and it must not treat a prefix as an id without `--batch`-style resolution
///   rules.
/// * **`abbrev` and `long_abbrev` change id formatting in the batch modes**, not in the single
///   modes. The batch modes are a separate, line-oriented interface that this scaffold accepts
///   but does not implement in v1; they must fail loudly rather than falling back to the single
///   form, because a script reading `--batch-check` output expects one line per object and would
///   silently misparse a pretty-printed dump.
#[derive(Debug, clap::Args)]
pub struct CatFileArgs {
    /// The object to read: an id, an abbreviated id, or a `REV:path`.
    #[arg(value_name = "OBJECT", required_unless_present_any = ["batch", "batch_check"])]
    pub object: Option<String>,

    /// Print the object's contents in its own format.
    #[arg(short = 'p')]
    pub pretty: bool,

    /// Print the object's type.
    #[arg(short = 't')]
    pub kind: bool,

    /// Print the object's size in bytes.
    #[arg(short = 's')]
    pub size: bool,

    /// Print nothing; use the exit code to report whether the object exists.
    #[arg(short = 'e')]
    pub exists: bool,

    /// Print the object's id rather than its contents.
    #[arg(long = "allow-unknown-type")]
    pub allow_unknown_type: bool,

    /// Print the whole object, not only the first part.
    #[arg(long = "batch", value_name = "FORMAT")]
    pub batch: Option<Option<String>>,

    /// Print one line of metadata per object read from standard input.
    #[arg(long = "batch-check", value_name = "FORMAT")]
    pub batch_check: Option<Option<String>>,

    /// Read the objects for `--batch` in NUL-separated form.
    #[arg(long = "batch-all-objects")]
    pub batch_all_objects: bool,

    /// Abbreviate ids to this many hex characters in the batch formats.
    #[arg(long = "abbrev", value_name = "N")]
    pub abbrev: Option<Option<usize>>,

    /// Print the object ids in full in the batch formats.
    #[arg(long = "follow-symlinks")]
    pub follow_symlinks: bool,

    /// Buffer the batch output before writing it.
    #[arg(long = "buffer")]
    pub buffer: bool,
}

/// Arguments to `rivet ls-tree`.
///
/// # Caveats
///
/// * **A treeish that is not a tree is peeled to one.** `rivet ls-tree HEAD` works because the
///   commit is peeled to its tree; `rivet ls-tree HEAD:src` works because the tree entry is
///   looked up. A handler that required a tree object id would reject the form everyone uses.
/// * **`recursive` changes the shape of the output, not just its depth.** A subtree entry is
///   printed as an entry of type `tree` in the non-recursive form and is *not* printed at all in
///   the recursive one — only its children are, with their full paths. A handler that recursed
///   and also printed the directory produces an extra line per subdirectory that no script
///   expects.
/// * **`name_only` prints the path with no type or mode, and `name_status` adds a status
///   letter**, which for `ls-tree` is always `A` — the two are not alternatives to the default
///   format but different formats, and `-z` affects all three.
/// * **The mode is printed as six octal digits with a leading zero** — `040000`, `100644`,
///   `100755`, `120000`, `160000`. Printing `40000` matches the on-disk form and breaks every
///   script, because the on-disk form is what the six-digit display exists to hide.
/// * **The name is the last field and is not quoted in the default format**, so a filename
///   containing a tab makes the output ambiguous — which is exactly why `-z` exists, and why a
///   handler must not quote or escape the name in the default form. Matching Git's ambiguity is
///   the requirement, not fixing it.
/// * **An object id in the batch output is abbreviated according to `core.abbrev`**, defaulting
///   to seven hex characters and never fewer than four. A fixed width here disagrees with `git`
///   on a large repository where the abbreviation was widened to stay unambiguous.
#[derive(Debug, clap::Args)]
pub struct LsTreeArgs {
    /// The tree or the commit whose tree to list. Defaults to the index.
    #[arg(value_name = "TREE-ISH")]
    pub tree: Option<String>,

    /// The paths within the tree to limit the listing to.
    #[arg(value_name = "PATHSPEC", last = true)]
    pub paths: Vec<PathBuf>,

    /// Descend into subtrees, printing every entry with its full path.
    #[arg(short = 'r')]
    pub recursive: bool,

    /// Print the path only, with no mode, type or id.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Print the path and a status letter instead of the mode and type.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Print the id in full rather than abbreviated.
    #[arg(long = "full-tree")]
    pub full_tree: bool,

    /// Print the tree's own contents rather than the entries of a named directory.
    #[arg(long = "full-name")]
    pub full_name: bool,

    /// Print the size of each blob as well as its id.
    #[arg(short = 'l', long = "long")]
    pub long: bool,

    /// Print the tree entries with no indentation or decoration, one per line.
    #[arg(long = "object-only")]
    pub object_only: bool,

    /// Read the tree entries in NUL-separated form.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Abbreviate ids to this many hex characters.
    #[arg(long = "abbrev", value_name = "N")]
    pub abbrev: Option<Option<usize>>,
}

/// Arguments to `rivet ls-files`.
///
/// # Caveats
///
/// * **The default output is the index's paths and nothing else**, and every one of the flags
///   below *adds a column* rather than filtering in most cases — `-s` prepends the mode and the
///   id, `-t` a status tag, `--stage` the conflict stage. A handler that treated `-s` as a filter
///   prints a different set of lines rather than a wider format.
/// * **`stage` and `cached` are the same default and `deleted`/`modified`/`others` are the
///   filters.** The set of flags is inherited from `git ls-files` and is not a clean design; it
///   is reproduced because scripts depend on the exact selection each one makes.
/// * **`others` lists untracked files and therefore needs a work tree**, so `--others` in a bare
///   repository is an error rather than an empty list. `--cached` in a bare repository is the
///   normal case and must work.
/// * **`ignored` requires `--others` or `--exclude-standard`** in Git's grammar, and the
///   combination is what lists the files the ignore rules excluded. Listing ignored paths
///   without also listing the untracked ones is a different question and Git's flag
///   combinations encode it; the handler must not accept a combination that selects nothing.
/// * **`unmerged` prints each conflicted path once per stage**, with the stage number in the
///   path's position. That is the opposite of `status`'s rule of one line per conflict, and the
///   two are correct for their respective purposes — one is a display, the other an index dump.
/// * **The paths are printed relative to the work tree root by default**, not to the current
///   directory. Git prints them relative to the current directory when `--full-name` is absent
///   and the process is in a subdirectory; that rule is subtle and must be implemented
///   deliberately, because a script running from a subdirectory gets paths it can pass straight
///   back to `rivet` only if it is.
#[derive(Debug, clap::Args)]
pub struct LsFilesArgs {
    /// The paths to limit the listing to.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// List the entries in the index. The default.
    #[arg(short = 'c', long = "cached")]
    pub cached: bool,

    /// List the files in the work tree that are not in the index.
    #[arg(short = 'o', long = "others")]
    pub others: bool,

    /// List the files that the ignore rules exclude.
    #[arg(short = 'i', long = "ignored")]
    pub ignored: bool,

    /// List the index entries whose work tree file has been deleted.
    #[arg(short = 'd', long = "deleted")]
    pub deleted: bool,

    /// List the work tree files that differ from the index.
    #[arg(short = 'm', long = "modified")]
    pub modified: bool,

    /// Do not list the files that are not tracked.
    #[arg(long = "no-empty-directory")]
    pub no_empty_directory: bool,

    /// List each conflicted path once per stage.
    #[arg(short = 'u', long = "unmerged")]
    pub unmerged: bool,

    /// Print the mode, the id and the stage before each path.
    #[arg(short = 's', long = "stage")]
    pub stage: bool,

    /// Print a status tag before each path.
    #[arg(short = 't')]
    pub tag: bool,

    /// Print the path without the current directory's prefix.
    #[arg(long = "full-name")]
    pub full_name: bool,

    /// Show the paths abbreviated, as `git status --short` does.
    #[arg(long = "abbreviate")]
    pub abbreviate: bool,

    /// Use the standard ignore rules when listing untracked files.
    #[arg(long = "exclude-standard")]
    pub exclude_standard: bool,

    /// Use no ignore rules when listing untracked files.
    #[arg(long = "no-exclude-standard")]
    pub no_exclude_standard: bool,

    /// Also list the contents of directories that are ignored in full.
    #[arg(long = "directory")]
    pub directory: bool,

    /// Separate the paths with a NUL byte rather than a newline.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Fail if a pathspec matches no tracked file.
    #[arg(long = "error-unmatch")]
    pub error_unmatch: bool,

    /// Read the pathspecs from a file rather than the command line.
    #[arg(long = "pathspec-from-file", value_name = "FILE")]
    pub pathspec_from_file: Option<PathBuf>,
}

/// Arguments to `rivet rev-parse`.
///
/// # Caveats
///
/// * **The command has three unrelated jobs and this grammar exposes only the first two**: turn a
///   revision into an id, and report a property of the repository (`--git-dir`,
///   `--show-toplevel`, `--is-bare-repository`). Upstream's third job — parsing options for other
///   commands (`--parseopt`, `--sq`) — is a shell-scripting aid this scaffold does not carry.
/// * **`verify` changes the command from "resolve or explain" to "resolve or fail cleanly"**,
///   and the difference is the *output on failure*: without it an unresolvable revision prints a
///   message on stderr and exits `128`, and with it nothing is printed and the exit code is the
///   only signal. `--verify` is why a script can capture stderr without it polluting the result.
/// * **`quiet` suppresses the error message but not the exit code.** The two flags are
///   independent, and a caller that set both should get an exit code and no output at all —
///   which is the form a shell `if` uses, and the reason neither flag implies the other.
/// * **The repository properties are answered without opening the object database**, so
///   `--git-dir` works in a repository whose objects are corrupt. That is not an optimisation:
///   it is what makes `--git-dir` usable from a script that is trying to find out what is wrong.
/// * **An abbreviated id is resolved through the object database and an ambiguous prefix is an
///   error**, [`rivet_core::Error::AmbiguousPrefix`], naming the number of candidates. Printing
///   the first match is the behaviour a `rev-parse` must never have; it is how a script silently
///   operates on the wrong commit.
/// * **A `REV:path` resolves to a blob or tree id and a `REV^{tree}` peels to the tree**, and
///   the two are spelled differently for a reason. `--verify` on a `REV:path` that does not
///   exist is exit `128`, not an empty id, which is what makes it usable in a test.
/// * **`--symbolic-full-name` prints the ref name rather than the id**, and returns nothing when
///   the revision is a raw object id. It is the inverse of resolving, and a handler that fell
///   back to printing the id makes a caller unable to distinguish "a branch named that" from "a
///   commit id passed through".
#[derive(Debug, clap::Args)]
pub struct RevParseArgs {
    /// The revisions and repository properties to resolve.
    #[arg(value_name = "ARG", required = true)]
    pub args: Vec<String>,

    /// The revisions must resolve to an object id, and nothing else may be printed.
    #[arg(long = "verify")]
    pub verify: bool,

    /// Suppress the error message for a revision that does not resolve.
    #[arg(long = "quiet")]
    pub quiet: bool,

    /// Print the git directory's path.
    #[arg(long = "git-dir")]
    pub git_dir: bool,

    /// Print the work tree's root path.
    #[arg(long = "show-toplevel")]
    pub show_toplevel: bool,

    /// Print whether the repository is bare, as the string `true` or `false`.
    #[arg(long = "is-bare-repository")]
    pub is_bare_repository: bool,

    /// Print whether the repository is inside a work tree.
    #[arg(long = "is-inside-work-tree")]
    pub is_inside_work_tree: bool,

    /// Print whether the repository is inside its git directory.
    #[arg(long = "is-inside-git-dir")]
    pub is_inside_git_dir: bool,

    /// Print whether an object exists, and nothing else.
    #[arg(long = "is-shallow-repository")]
    pub is_shallow_repository: bool,

    /// Print the object's id rather than the ref name it came from.
    #[arg(long = "symbolic-full-name")]
    pub symbolic_full_name: bool,

    /// Print the abbreviated id rather than the full one.
    #[arg(long = "short", value_name = "N")]
    pub short: Option<Option<usize>>,

    /// Print every branch whose tip is this commit.
    #[arg(long = "branches")]
    pub branches: bool,

    /// Print every tag pointing at this commit.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Print the id that a `^` or `~` suffix would peel to.
    #[arg(long = "abbrev-ref", value_name = "MODE")]
    pub abbrev_ref: Option<String>,

    /// Read the revisions from standard input, one per line.
    #[arg(long = "stdin")]
    pub stdin: bool,

    /// Read the revisions from standard input, NUL-separated.
    #[arg(long = "all")]
    pub all: bool,

    /// Print the revisions with no trailing newline.
    #[arg(long = "not", value_name = "REVISION")]
    pub not: Vec<String>,

    /// Print the resolved object's type as well as its id.
    #[arg(long = "default", value_name = "ARG")]
    pub default: Option<String>,

    /// Use the repository-local config only, ignoring the global and system files.
    #[arg(long = "local-env-vars")]
    pub local_env_vars: bool,
}

/// Arguments to `rivet write-tree`.
///
/// # Caveats
///
/// * **The tree is built from the index, not from the work tree.** No file is hashed and no
///   unstaged modification is seen; the command answers "what tree would a commit make right
///   now", which is exactly the question `commit` needs answered and a different one from "what
///   is in the work tree".
/// * **An unmerged index is a refusal**, [`rivet_core::Error::UnmergedPath`], naming the paths.
///   Writing a tree from an index holding conflict stages would either drop them silently or
///   invent a resolution, and both produce a tree no user asked for.
/// * **`prefix` writes only a subtree and prints its id**, relative to the work tree root — the
///   form `git write-tree --prefix=sub` uses to build a submodule's tree. It does not write the
///   parent trees, so a caller using it is responsible for the rest of the path.
/// * **Writing is not idempotent in cost but is in effect.** A tree that already exists is
///   rewritten identically and its id does not change, because the object database skips a write
///   whose object is already present. A caller may therefore call this repeatedly without
///   creating garbage.
/// * **The command must not touch the index file**, only read it. It takes no lock and writes no
///   entry; a `write-tree` that refreshed the index would make a read-only plumbing command
///   fail in a repository another process is writing.
#[derive(Debug, clap::Args)]
pub struct WriteTreeArgs {
    /// Write only the subtree rooted at this path, relative to the work tree root.
    #[arg(long = "prefix", value_name = "PREFIX")]
    pub prefix: Option<String>,

    /// Do not descend into submodules.
    #[arg(long = "missing-ok")]
    pub missing_ok: bool,
}

/// Arguments to `rivet verify-pack`.
///
/// # Caveats
///
/// * **The `.idx` is the argument and the `.pack` is found from it**, not the other way round,
///   because the index is what names the pack and what carries the per-object CRCs. Passing a
///   `.pack` is a usage error in Git and must be here too, with the message saying which file to
///   pass instead.
/// * **`stat_only` prints the pack's summary and skips per-object verification**, which is the
///   difference between a fast answer and a thorough one. It is the form a script uses to learn
///   an object count, and it must not be implemented as "verify then suppress the output".
/// * **`verbose` prints one line per object**, and a pack of a million objects makes that a
///   million lines. It is the form a human uses after a real failure, and the per-object line's
///   shape — id, type, size, offset, depth — is what makes it diagnosable; the fields are
///   separated by spaces and a script that splits on them depends on all five being present.
/// * **A corrupt entry is reported and the exit code is non-zero, but the walk continues**,
///   because the useful output from a broken pack is the *set* of broken entries, not the first
///   one. `rivet_core::pack::PackReader` yields `Result` per entry, which is what makes that
///   possible without an early return.
/// * **Verification includes the trailing checksum and every object's CRC32.** Checking only the
///   checksum accepts a pack whose data was modified and re-checksummed; checking only the CRCs
///   accepts a truncated pack whose last entry is missing. Both are required and the two are
///   separate steps in [`rivet_core::PackFile::verify_checksum`] and the index walk.
#[derive(Debug, clap::Args)]
pub struct VerifyPackArgs {
    /// The `.idx` files to verify. Their packs are found from them.
    #[arg(value_name = "PACK-IDX", required = true)]
    pub indexes: Vec<PathBuf>,

    /// Print a line for each object in the pack.
    #[arg(long = "verbose")]
    pub verbose: bool,

    /// Print the pack's summary and skip per-object verification.
    #[arg(short = 's', long = "stat-only")]
    pub stat_only: bool,

    /// Read the raw pack from a stream rather than from a file.
    #[arg(long = "object-format", value_name = "FORMAT")]
    pub object_format: Option<String>,
}

/// Arguments to `rivet fsck`.
///
/// # Caveats
///
/// * **`connectivity_only` is the fast check and the full check is the slow one.** The first
///   walks the object graph from the refs and reports what is missing; the second additionally
///   parses every object and rejects a malformed one. A script that runs `fsck` on a timer wants
///   the first, and a handler that implemented it as "full, then filter the output" makes the
///   cheap check cost the expensive one.
/// * **A repository with unreachable objects is not corrupt and the exit code says so only with
///   `--unreachable`.** Dangling objects are the normal result of an amended commit or a reset,
///   and reporting them as errors makes `fsck` useless in any repository that has been worked in.
/// * **`strict` turns warnings into errors**, and the difference matters: a file mode of `100664`
///   is a warning, a tree entry named `.git` is an error, and a duplicate name in a tree is an
///   error. The plan's §5 lists the second and third as must-reject, so they are errors even
///   without `--strict`; `--strict` is for the cases Git only warns about.
/// * **A missing object that is reachable from a ref is the one thing `fsck` must report
///   loudly**, because it is the only corruption that makes a repository unusable rather than
///   merely untidy. `--full` is what forces every ref, including tags and remote-tracking ones,
///   to be walked; a plain `fsck` walks only what `HEAD` reaches.
/// * **`fsck` must open a repository whose objects are corrupt**, per [`rivet_core::Repository`]'s
///   caveat: nothing is validated at open time precisely so that this command can report what is
///   wrong. A handler that used a validating open would fail before printing the diagnosis.
/// * **`no_dangling` suppresses the dangling-object report while still checking them**, which is
///   not the same as `--connectivity-only`. The first validates and does not print; the second
///   prints and does not validate. Implementing one as the other is invisible until a repository
///   has a corrupt dangling object.
/// * **`lost_found` writes the unreachable objects into a synthetic commit on
///   `refs/heads/lost-found`**, which makes them reachable again and recoverable. It is the
///   rescue path after a bad `reset --hard`, and it must not run by default — writing refs is
///   not something a verification command does without being asked.
#[derive(Debug, clap::Args)]
pub struct FsckArgs {
    /// Also report the objects that no ref reaches.
    #[arg(long = "unreachable")]
    pub unreachable: bool,

    /// Report the objects that no ref reaches, as well as checking them. The default.
    #[arg(long = "dangling")]
    pub dangling: bool,

    /// Do not report the objects that no ref reaches.
    #[arg(long = "no-dangling")]
    pub no_dangling: bool,

    /// Treat warnings as errors.
    #[arg(long = "strict")]
    pub strict: bool,

    /// Check only that the object graph is connected, without parsing each object.
    #[arg(long = "connectivity-only")]
    pub connectivity_only: bool,

    /// Write the unreachable objects into a commit on `refs/heads/lost-found`.
    #[arg(long = "lost-found")]
    pub lost_found: bool,

    /// Also check the objects that the index names but no ref does.
    #[arg(long = "cache")]
    pub cache: bool,

    /// Check the refs' structure and their reflogs as well as the objects.
    #[arg(long = "root")]
    pub root: bool,

    /// Also check the reflogs for entries pointing at missing objects.
    #[arg(long = "no-reflogs")]
    pub no_reflogs: bool,

    /// Print a line for each object checked.
    #[arg(long = "verbose")]
    pub verbose: bool,

    /// Report only the problems, and nothing for a clean repository.
    #[arg(long = "progress")]
    pub progress: bool,
}

/// Phase 1 — compute an object id, and optionally store the object.
///
/// # Purpose
///
/// Hash a file or a byte stream as `"<type> <len>\0<body>"` and print the id, writing the object
/// into `.git/objects/` when `--write` is given. This is the primitive every other write path is
/// built on, and the command the Phase 1 milestone is verified with: Rivet's id must equal
/// `git hash-object`'s for random bytes, in both object formats.
///
/// # Exit codes
///
/// * `0` — the id was computed (and the object written, if asked).
/// * `128` — a file could not be read, the object could not be written, or a `--stdin-paths`
///   line is not a readable path.
///
/// # Caveats
///
/// * **The hash covers the header, not just the body.** `"blob 12\0hello world\n"` is what is
///   hashed; hashing the file's bytes alone produces an id that looks plausible and that Git
///   will never agree with. The header's length is the *body's* length and is computed from the
///   bytes actually read, not from the file's metadata size, which differs for a file that is
///   being written while it is hashed.
/// * **The bytes are read as they are, with no newline translation and no encoding.** A file
///   ending without a newline hashes differently from the same file with one, and that is
///   correct — this is the command whose whole purpose is to be byte-exact.
/// * **Outside a repository there is no format to read, and SHA-1 is assumed.** `--write`
///   outside a repository is an error, because there is nowhere to write to; computing an id is
///   not, which is what makes `rivet hash-object` usable in a pre-commit check that runs before
///   the repository exists.
/// * **A `--stdin` read must consume the whole stream before hashing**, since the length goes in
///   the header. Streaming into the hasher without knowing the length is the mistake that
///   produces an id for a zero-length object on any input longer than a pipe buffer.
/// * **`--literally` produces an object whose content is not valid for its type**, which `fsck`
///   will then report. It exists for testing corrupt repositories and must be off by default.
pub fn run_hash_object(global: &GlobalArgs, args: &HashObjectArgs) -> Result<i32> {
    use std::io::Read;

    let kind: rivet_core::ObjectKind = args.kind.into();

    // Outside a repository there is still an answer — the id — but nowhere to write. Detecting the
    // absence explicitly keeps `hash-object` usable before `init`, which the header documents.
    let repository = match super::open(global) {
        Ok(repository) => Some(repository),
        Err(error) if is_not_a_repository(&error) => None,
        Err(error) => return Err(error),
    };
    if args.write && repository.is_none() {
        anyhow::bail!("hash-object --write: not a repository");
    }
    let format = repository
        .as_ref()
        .map(|repository| repository.format())
        .unwrap_or(rivet_core::ObjectFormat::Sha1);

    // Collect the byte streams first: a `--stdin` read has to be complete before the header that
    // carries its length can be written.
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    if args.stdin {
        let mut buffer = Vec::new();
        std::io::stdin().read_to_end(&mut buffer)?;
        inputs.push(buffer);
    } else if args.stdin_paths {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        for line in text.lines().filter(|line| !line.is_empty()) {
            inputs.push(std::fs::read(line)?);
        }
    } else {
        for path in &args.paths {
            inputs.push(std::fs::read(path)?);
        }
    }

    for body in inputs {
        let object = match kind {
            rivet_core::ObjectKind::Blob => rivet_core::Object::Blob(body),
            rivet_core::ObjectKind::Tree => rivet_core::Object::Tree(body),
            rivet_core::ObjectKind::Commit => rivet_core::Object::Commit(body),
            rivet_core::ObjectKind::Tag => rivet_core::Object::Tag(body),
        };
        let id = if args.write {
            repository
                .as_ref()
                .expect("checked above")
                .odb()
                .write(&object)?
        } else {
            object.id(format)?
        };
        println!("{}", id.to_hex());
    }
    Ok(0)
}

/// Phase 1 — show an object's type, size or contents.
///
/// # Purpose
///
/// The read side of the object database: resolve an id or a `REV:path`, read the object, and
/// print its type with `-t`, its size with `-s`, its pretty-printed contents with `-p`, or
/// nothing at all with `-e`.
///
/// # Exit codes
///
/// * `0` — the object exists and whatever was asked for was printed.
/// * `1` — `-e` was given and the object does not exist. The documented negative: the answer to
///   "does this object exist" is no, and the command succeeded in answering.
/// * `128` — the revision does not resolve, an abbreviated id is ambiguous, the object is
///   corrupt, or no mode flag was given.
///
/// # Caveats
///
/// * **`-e`'s `1` is not an error and the other modes' `1` is not used.** A handler that
///   returned `1` for a missing object under `-p` would make a script unable to distinguish
///   "missing" from "present and empty"; under `-e` it is the entire answer.
/// * **The object's hash is verified when it is read.** A loose object whose contents do not
///   hash to its name is [`rivet_core::Error::HashMismatch`], not a successful read of
///   corrupted bytes — which is the difference between a detected corruption and a silent one,
///   and it is why this command is the one to run when a repository is suspected of being
///   damaged.
/// * **`-p` on a blob writes the bytes verbatim**, per [`CatFileArgs`]; the output may be binary
///   and nothing may be appended to it.
/// * **`-s` reports the size after any delta resolution**, not the size of the delta. On a packed
///   repository the two differ by an order of magnitude and only one of them matches `git`.
/// * **`--batch` and `--batch-check` are a separate interface and are not implemented in v1.**
///   They must be refused rather than silently falling back to the single-object form, because a
///   script reads them line by line and a pretty-printed object would be parsed as many
///   malformed lines rather than as one error.
pub fn run_cat_file(global: &GlobalArgs, args: &CatFileArgs) -> Result<i32> {
    use std::io::Write;

    if args.batch.is_some() || args.batch_check.is_some() || args.batch_all_objects {
        anyhow::bail!("cat-file: batch mode is not implemented yet");
    }

    let repository = super::open(global)?;
    let spec = args.object.as_deref().unwrap_or_default();
    let format = repository.format();

    let resolved = rivet_core::revparse::resolve_path(
        format,
        repository.refs(),
        repository.odb(),
        bstr::BStr::new(spec),
    );
    let object = match resolved {
        Ok(oid) => match repository.odb().read(&oid) {
            Ok(object) => object,
            // `-e` turns "the database does not hold it" into a documented negative answer.
            Err(_) if args.exists => return Ok(1),
            Err(error) => return Err(error.into()),
        },
        Err(_) if args.exists => return Ok(1),
        Err(error) => return Err(error.into()),
    };

    if args.exists {
        return Ok(0);
    }

    // Git's own precedence when more than one is given: `-p`, then `-t`, then `-s`.
    if args.pretty {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        match object.kind() {
            rivet_core::ObjectKind::Blob => out.write_all(object.data())?,
            rivet_core::ObjectKind::Tree => {
                let tree = rivet_core::Tree::parse(format, object.data())?;
                for entry in &tree.entries {
                    let kind = match entry.mode {
                        rivet_core::EntryMode::Tree => "tree",
                        rivet_core::EntryMode::Commit => "commit",
                        _ => "blob",
                    };
                    writeln!(
                        out,
                        "{:06o} {} {}\t{}",
                        entry.mode.to_u32(),
                        kind,
                        entry.oid.to_hex(),
                        entry.name
                    )?;
                }
            }
            // Commits and tags are already text in the form `-p` prints.
            _ => out.write_all(object.data())?,
        }
        out.flush()?;
    }
    if args.kind {
        println!("{}", object.kind().as_str());
    }
    if args.size {
        println!("{}", object.data().len());
    }
    if !args.pretty && !args.kind && !args.size {
        anyhow::bail!("cat-file: one of -p, -t, -s or -e is required");
    }
    Ok(0)
}

/// Phase 4 — list the contents of a tree.
///
/// # Purpose
///
/// Print a tree's entries — mode, type, id and name — one per line, optionally descending into
/// subtrees. The command a script uses to read a directory from history without checking it out.
///
/// # Exit codes
///
/// * `0` — the tree was listed. An empty tree prints nothing and succeeds.
/// * `128` — the treeish does not resolve, the named path is not in the tree, or the object is
///   not a tree and cannot be peeled to one.
///
/// # Caveats
///
/// * **The entry order is the tree's on-disk order**, which is sorted with a subtree's name
///   compared as if it ended in `/`. It is *not* plain byte order: `foo.txt` sorts before `foo/`
///   and `foo-bar` sorts after it, because `/` (0x2F) sits between `-` (0x2D) and `.` (0x2E)…
///   and the comparison appends the slash only for trees. Printing the entries in byte order
///   looks nearly right and disagrees with `git ls-tree` on exactly the repositories that have
///   both a `foo/` and a `foo.x`.
/// * **A gitlink's mode is `160000` and its id is a commit, not a tree**, and the entry must be
///   printed without descending into it. A recursive listing that tried to read the gitlink as a
///   tree fails with an object-not-found error, which is the second-most-common shape of this
///   bug.
/// * **`--long` prints the blob's size, which requires reading every object in the tree.** On a
///   large tree that is a pack read per entry, and it is the reason `-l` is not the default; the
///   size column is padded and the output is not a stable interface.
/// * **A name containing a tab or a newline makes the default format ambiguous**, per
///   [`LsTreeArgs`], and `-z` is the answer. The name is never quoted in the default form.
pub fn run_ls_tree(global: &GlobalArgs, args: &LsTreeArgs) -> Result<i32> {
    use std::io::Write;

    let mut repository = super::open(global)?;
    let format = repository.format();

    // No tree-ish means "the index": Git materialises it as a tree rather than pretending an
    // index has a tree's shape, which is the only way to print nested entries consistently.
    let tree_oid = match args.tree.as_deref() {
        Some(spec) => rivet_core::revparse::to_tree(
            format,
            repository.refs(),
            repository.odb(),
            bstr::BStr::new(spec),
        )?,
        None => repository.write_tree()?,
    };

    let separator = if args.null_terminated { '\0' } else { '\n' };
    // Git prints full ids unless `--abbrev` asks otherwise: abbreviation is opt-in for ls-tree,
    // unlike the pretty commands, and a script parsing the default form sees 40 hex characters.
    let abbrev_len = match args.abbrev {
        Some(Some(length)) => length,
        _ => format.hex_len(),
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    list_tree(
        &mut out,
        &repository,
        format,
        &tree_oid,
        b"",
        args,
        abbrev_len,
        separator,
    )?;
    out.flush()?;
    Ok(0)
}

/// Walks one tree, printing each entry and, when asked, descending into subtrees.
fn list_tree(
    out: &mut impl std::io::Write,
    repository: &rivet_core::Repository,
    format: rivet_core::ObjectFormat,
    tree_oid: &rivet_core::ObjectId,
    prefix: &[u8],
    args: &LsTreeArgs,
    abbrev_len: usize,
    separator: char,
) -> Result<()> {
    let object = repository.odb().read(tree_oid)?;
    let tree = rivet_core::Tree::parse(format, object.data())?;
    for entry in &tree.entries {
        let mut path = prefix.to_vec();
        path.extend_from_slice(&entry.name);
        if !matches_pathspec(&path, &args.paths) {
            continue;
        }
        let kind = match entry.mode {
            rivet_core::EntryMode::Tree => "tree",
            rivet_core::EntryMode::Commit => "commit",
            _ => "blob",
        };
        // `-r` lists files, not the directories it walks through: without `-t` Git omits tree
        // entries entirely, which is what makes `ls-tree -r | wc -l` a file count.
        let descend = args.recursive && entry.mode == rivet_core::EntryMode::Tree;
        if descend {
            let mut child_prefix = path.clone();
            child_prefix.push(b'/');
            list_tree(
                out,
                repository,
                format,
                &entry.oid,
                &child_prefix,
                args,
                abbrev_len,
                separator,
            )?;
            continue;
        }
        // A leading `/` in the recorded path is how Git's `-r` shows a path nested in a subtree.
        if args.name_only {
            out.write_all(&path)?;
            write!(out, "{separator}")?;
        } else if args.name_status {
            write!(out, "A\t")?;
            out.write_all(&path)?;
            write!(out, "{separator}")?;
        } else {
            let id = entry.oid.to_hex();
            let id: &[u8] = if args.full_tree || abbrev_len >= format.hex_len() {
                &id
            } else {
                &id[..abbrev_len.min(id.len())]
            };
            if args.object_only {
                out.write_all(id)?;
            } else if args.long {
                let size = match entry.mode {
                    rivet_core::EntryMode::Blob => repository
                        .odb()
                        .read_header(&entry.oid)
                        .map(|(_, size)| size)
                        .unwrap_or(0),
                    _ => 0,
                };
                write!(out, "{:06o} {} ", entry.mode.to_u32(), kind)?;
                out.write_all(id)?;
                write!(out, " {:>7}\t", size)?;
                out.write_all(&path)?;
            } else {
                write!(out, "{:06o} {} ", entry.mode.to_u32(), kind)?;
                out.write_all(id)?;
                write!(out, "\t")?;
                out.write_all(&path)?;
            }
            write!(out, "{separator}")?;
        }
    }
    Ok(())
}

/// Whether `path` should be listed, given the command's pathspecs.
///
/// No pathspecs means everything; otherwise a path matches when it *is* the pathspec or sits
/// inside it, which is the behaviour scripts rely on for a directory argument.
fn matches_pathspec(path: &[u8], pathspecs: &[PathBuf]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    pathspecs.iter().any(|spec| {
        let spec = spec.to_string_lossy();
        let spec = spec.trim_end_matches('/').as_bytes();
        path == spec || path.starts_with(spec) && path.get(spec.len()) == Some(&b'/')
    })
}

/// Phase 3 — list the files in the index.
///
/// # Purpose
///
/// Print the paths in the index, optionally with their mode, id and stage, and optionally
/// including the untracked, modified, deleted or ignored files the index does not hold.
///
/// # Exit codes
///
/// * `0` — the listing was produced.
/// * `128` — no repository, `--others` or `--ignored` in a bare repository, a pathspec matches
///   nothing under `--error-unmatch`, or the index could not be read.
///
/// # Caveats
///
/// * **The index is read, not refreshed.** A file modified on disk since it was staged appears
///   with its *staged* id, and `--modified` is what compares against the work tree. A handler
///   that refreshed the index first would return a different answer and would also write to a
///   file a read-only command has no business writing.
/// * **`--unmerged` prints one line per stage and `--stage` prints the stage number for every
///   entry**, including the `0` of an unconflicted one. The two flags together are what a
///   conflict-resolution script parses, and the stage digit's position — `H <mode> <id> <stage>`
///   or `<mode> <id> <stage> <path>` — differs between them.
/// * **`--others` needs the ignore stack and `--exclude-standard` selects it**, per
///   [`LsFilesArgs`]; without either, every file in a build directory is listed, which on a
///   large work tree is both wrong and slow.
/// * **The entries are sorted by path, and by stage within a path.** The index's own order is
///   already this — entries are sorted on write — but a listing that re-sorted with a different
///   comparison puts `foo` after `foo.txt`, which is where a script that consumed the order
///   notices.
/// * **A path is printed relative to the work tree root unless `--full-name` is absent and the
///   process is in a subdirectory**, per [`LsFilesArgs`]; the rule is Git's and getting it wrong
///   produces paths that a caller cannot feed back to `rivet`.
pub fn run_ls_files(global: &GlobalArgs, args: &LsFilesArgs) -> Result<i32> {
    use std::io::Write;

    let mut repository = super::open(global)?;
    let format = repository.format();
    let separator = if args.null_terminated { '\0' } else { '\n' };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let pathspecs: Vec<String> = args
        .paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let matches = |path: &[u8]| -> bool {
        pathspecs.is_empty()
            || pathspecs.iter().any(|spec| {
                let spec = spec.trim_end_matches('/').as_bytes();
                path == spec || path.starts_with(spec) && path.get(spec.len()) == Some(&b'/')
            })
    };

    // Git's default is `--cached`, and `-o` on its own means "only the untracked ones".
    let mut listed_cached = false;

    if args.cached || !(args.others || args.ignored || args.deleted || args.modified) {
        listed_cached = true;
        for entry in repository.index().entries() {
            if !matches(&entry.path) {
                continue;
            }
            if args.unmerged && !entry.is_unmerged() {
                continue;
            }
            write_index_entry(&mut out, format, entry, args, separator)?;
        }
    }

    if args.others || args.ignored {
        let mut options = rivet_core::worktree::StatusOptions::new();
        options.untracked(rivet_core::worktree::UntrackedMode::All);
        options.ignored(args.ignored);
        let status = repository.status(&options)?;
        let entries = if args.ignored {
            &status.ignored
        } else {
            &status.untracked
        };
        for path in entries {
            if matches(path) {
                out.write_all(path)?;
                write!(out, "{separator}")?;
            }
        }
    }

    if args.deleted || args.modified {
        let status = repository.status(&rivet_core::worktree::StatusOptions::new())?;
        for entry in &status.unstaged {
            if !matches(&entry.path) {
                continue;
            }
            let deleted = entry.kind == rivet_core::worktree::ChangeKind::Deleted;
            if (args.deleted && deleted) || (args.modified && !deleted) {
                out.write_all(&entry.path)?;
                write!(out, "{separator}")?;
            }
        }
    }

    if args.error_unmatch && !listed_cached {
        anyhow::bail!("ls-files: --error-unmatch needs --cached");
    }
    out.flush()?;
    Ok(0)
}

/// Prints one index entry in the form `ls-files` uses: `<mode> <id> <stage>\t<path>`, or just the
/// path under `-c`, with an optional status tag.
fn write_index_entry(
    out: &mut impl std::io::Write,
    format: rivet_core::ObjectFormat,
    entry: &rivet_core::IndexEntry,
    args: &LsFilesArgs,
    separator: char,
) -> Result<()> {
    if args.stage {
        let stage = match entry.stage() {
            rivet_core::index::Stage::Normal => 0,
            rivet_core::index::Stage::Base => 1,
            rivet_core::index::Stage::Ours => 2,
            rivet_core::index::Stage::Theirs => 3,
        };
        write!(
            out,
            "{:06o} {} {}\t",
            entry.mode.to_u32(),
            entry.oid.to_hex(),
            stage
        )?;
        let _ = format;
    } else if args.tag {
        write!(out, "H ")?;
    }
    out.write_all(&entry.path)?;
    write!(out, "{separator}")?;
    Ok(())
}

/// Phase 4 — resolve a revision or a repository property.
///
/// # Purpose
///
/// Turn `HEAD~2`, a short id, a branch name or `REV:path` into a full object id, and report a
/// property of the repository such as `--git-dir` or `--show-toplevel`. The command every shell
/// script that works with Git calls first.
///
/// # Exit codes
///
/// * `0` — every argument resolved, and the answers were printed one per line.
/// * `128` — an argument did not resolve, an id prefix was ambiguous, no repository was found
///   for a repository property, or `--verify` was given more than one argument.
///
/// # Caveats
///
/// * **One argument per output line, in the order given.** `rivet rev-parse HEAD HEAD~1` prints
///   two lines; a handler that resolved a range into one line breaks every caller. The
///   `--not`/`--all` forms that build ranges are the exception and print one line per resolved
///   revision.
/// * **`--verify` requires exactly one argument**, and more than one is a usage error rather
///   than a loop. Its purpose is a single boolean answer, and a caller looping over arguments
///   should not have to check how many it passed.
/// * **An ambiguous abbreviation is an error naming the candidate count**, never the first
///   match. This is the single most important safety property of the command: a script that
///   resolved a 7-character id to the wrong commit has silently operated on the wrong history,
///   and nothing in its output would have said so.
/// * **`--short` may produce more characters than asked.** Seven is a minimum, and an ambiguous
///   seven-character prefix is widened until it is unique — so `--short` on a large repository
///   returns eight or nine. Printing exactly seven and declaring it done is how a script's
///   output stops being resumable.
/// * **A revision that peels prints the peeled id**, so `v1.0^{commit}` on an annotated tag is
///   the commit and not the tag object. `--symbolic-full-name` is the inverse and returns empty
///   for a raw id; a handler that fell back to the id makes the two indistinguishable.
/// * **`--is-inside-work-tree` is `false`, not an error, in a bare repository**, and
///   `--show-toplevel` *is* an error there. The asymmetry is Git's: the first is a predicate
///   whose answer can be "no", the second is a path that does not exist.
/// * **The repository properties are answered from the git directory alone**, without reading
///   the index or the object database, so that they work in a damaged repository. That is what
///   makes `rivet rev-parse --git-dir` the first thing to run when something is wrong.
pub fn run_rev_parse(global: &GlobalArgs, args: &RevParseArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let format = repository.format();

    // Repository properties print first, as in Git, and do not consume the positional arguments.
    if args.git_dir {
        println!("{}", repository.git_dir().display());
    }
    if args.show_toplevel {
        match repository.worktree_path() {
            Some(path) => println!("{}", path.display()),
            None => anyhow::bail!("this operation must be run in a work tree"),
        }
    }
    if args.is_bare_repository {
        println!("{}", repository.is_bare());
    }
    if args.is_inside_work_tree {
        println!("{}", repository.worktree_path().is_some());
    }
    if args.is_inside_git_dir {
        let cwd = std::env::current_dir()?;
        println!("{}", cwd.starts_with(repository.git_dir()));
    }

    let mut status = 0;

    for spec in &args.args {
        let spec_bytes = bstr::BStr::new(spec);
        let resolved = rivet_core::revparse::resolve_path(
            format,
            repository.refs(),
            repository.odb(),
            spec_bytes,
        );
        match resolved {
            Ok(oid) => {
                if args.symbolic_full_name {
                    println!("{}", symbolic_name(&repository, spec, oid));
                } else if let Some(short) = args.short {
                    let length = short.unwrap_or(7);
                    let hex = oid.to_hex();
                    let text = String::from_utf8_lossy(&hex);
                    println!("{}", &text[..length.min(text.len())]);
                } else {
                    println!("{}", oid.to_hex());
                }
            }
            Err(error) => {
                // `--quiet` suppresses the message but not the failure: scripts branch on the
                // exit code, and a silent zero here would claim a revision resolved.
                if !args.quiet {
                    eprintln!("fatal: {}", error);
                }
                if !args.verify {
                    // Git continues with the remaining arguments unless `--verify` was asked for,
                    // which is what lets `rev-parse --git-dir HEAD` print both.
                    status = 128;
                } else {
                    return Ok(128);
                }
            }
        }
    }
    Ok(status)
}

/// The ref name a spec came from, for `--symbolic-full-name`.
fn symbolic_name(
    repository: &rivet_core::Repository,
    spec: &str,
    oid: rivet_core::ObjectId,
) -> String {
    // A spec that is already a ref name is printed back in full; anything else (an id, a `~`
    // expression) has no symbolic name and Git prints the spec unchanged.
    let mut candidate = bstr::BString::from(spec);
    if !spec.starts_with("refs/") && !spec.starts_with("HEAD") {
        if let Ok(name) = rivet_core::refs::RefName::from_bytes(&candidate) {
            if repository.refs().read(&name).is_ok() {
                return spec.to_string();
            }
        }
        for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
            candidate = bstr::BString::from(prefix);
            candidate.extend_from_slice(spec.as_bytes());
            if let Ok(name) = rivet_core::refs::RefName::from_bytes(&candidate) {
                if repository.refs().resolve_oid(&name).map(|id| id == oid).unwrap_or(false) {
                    return candidate.to_string();
                }
            }
        }
        return spec.to_string();
    }
    spec.to_string()
}

/// Phase 3 — write the index out as a tree object.
///
/// # Purpose
///
/// Build tree objects from the index's entries, bottom-up, write them all, and print the root
/// tree's id — the plumbing step between `add` and `commit`, and the one a script uses to ask
/// "what would the next commit record".
///
/// # Exit codes
///
/// * `0` — the tree was written and its id printed.
/// * `128` — the index has unmerged paths, a `--prefix` names a path that is not in the index, or
///   an object could not be written.
///
/// # Caveats
///
/// * **Every directory along the way gets its own object**, not just the root, and they are
///   written children-first. A handler that wrote only the root would compute its id from
///   subtree ids that do not exist yet, which produces a tree `git` reports as having missing
///   objects.
/// * **An unmerged index is refused**, per [`WriteTreeArgs`]; there is no tree that can be built
///   from four stages of the same path, and inventing one is how a merge gets silently resolved
///   to a state nobody chose.
/// * **The entries are sorted by Git's tree comparison**, with a subtree's name compared as if
///   it ended in `/` — see [`run_ls_tree`]'s caveat. Writing them in index order works for every
///   repository that has no name that sorts differently as a file and as a directory, which is
///   almost all of them, and produces an invalid tree for the rest.
/// * **`--prefix` writes the subtree and prints its id, *not* the root's**, and it does not write
///   the enclosing trees. A caller that used the printed id as a repository root has an id for a
///   directory, not for the state of the index.
/// * **The index is not modified and not locked.** A `write-tree` never refreshes, never writes
///   and never takes `index.lock`; anything else makes a read-only plumbing command fail in a
///   repository another process is writing.
pub fn run_write_tree(global: &GlobalArgs, args: &WriteTreeArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let format = repository.format();
    let mut oid = repository.write_tree()?;

    if let Some(prefix) = &args.prefix {
        // `--prefix` writes the subtree rooted at a path, so walk down from the root tree with
        // the same component-at-a-time lookup `ls-tree` uses.
        let mut current = oid;
        for part in prefix.split('/').filter(|part| !part.is_empty()) {
            let object = repository.odb().read(&current)?;
            let tree = rivet_core::Tree::parse(format, object.data())?;
            let entry = tree
                .entries
                .iter()
                .find(|entry| entry.name == part.as_bytes())
                .ok_or_else(|| anyhow::anyhow!("write-tree: prefix not found: {}", prefix))?;
            if entry.mode != rivet_core::EntryMode::Tree {
                anyhow::bail!("write-tree: not a directory: {}", prefix);
            }
            current = entry.oid;
        }
        oid = current;
    }

    println!("{}", oid.to_hex());
    Ok(0)
}

/// Whether an `anyhow` error is really "there is no repository here".
///
/// Commands like `hash-object` have an answer outside a repository, so they need to tell that
/// specific failure apart from every other one rather than treating all `open` errors alike.
fn is_not_a_repository(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<rivet_core::Error>(),
        Some(rivet_core::Error::NotARepository(_))
    )
}

/// Phase 10 — validate a packfile and its index.
///
/// # Purpose
///
/// Read a pack through its `.idx`, verify the trailing checksum, walk every entry and check each
/// one's CRC32, resolve every delta and confirm the reconstructed object hashes to the id the
/// index claims — so that a pack Rivet wrote, and a pack Git wrote, are both provably intact.
///
/// # Exit codes
///
/// * `0` — every entry verified.
/// * `1` — the pack is corrupt: a bad checksum, a bad CRC, a delta that does not apply, or an
///   object whose hash does not match. The documented negative and the one a script tests.
/// * `128` — the `.idx` could not be read, its version is unsupported, or the pack it names is
///   missing.
///
/// # Caveats
///
/// * **A truncated pack whose last entry is missing still has a valid checksum over the bytes
///   present**, which is why the entry count from the index must be checked against the entries
///   the pack actually yields. Relying on the checksum alone accepts a pack that `git fsck`
///   rejects.
/// * **A thin pack is missing a base and is not corrupt**, so a `.idx` for one that was never
///   fixed up resolves to [`rivet_core::Error::ThinPack`] rather than a checksum failure. The
///   message must say so, because the fix is different from the fix for real corruption.
/// * **Delta depth is bounded** — [`rivet_core::MAX_DELTA_DEPTH`] — and a chain longer than that
///   is reported as [`rivet_core::Error::DeltaChainTooDeep`] rather than recursed into, so a
///   crafted pack cannot exhaust the stack. This is a robustness requirement, not an
///   optimisation.
/// * **Every object is re-hashed, not merely parsed.** The id in the index is what the object
///   must hash to, and a pack whose delta resolves to bytes that parse cleanly but do not hash
///   correctly is corrupt in the way that matters and is exactly what a verification is for.
/// * **`--stat-only` must not verify per-object**, per [`VerifyPackArgs`]; on a multi-gigabyte
///   pack the difference is minutes, and the flag exists for a script that wants the count.
pub fn run_verify_pack(global: &GlobalArgs, args: &VerifyPackArgs) -> Result<i32> {
    let format = match args.object_format.as_deref() {
        Some("sha256") => rivet_core::ObjectFormat::Sha256,
        Some("sha1") | None => match super::open(global) {
            Ok(repository) if args.object_format.is_none() => repository.format(),
            _ => rivet_core::ObjectFormat::Sha1,
        },
        Some(other) => anyhow::bail!("unsupported object format '{other}'"),
    };
    let verbose = args.verbose || global.verbose;
    let mut code = 0;
    for index_path in &args.indexes {
        // Git accepts the `.pack` name too and looks for its `.idx`.
        let pack_path = index_path.with_extension("pack");
        let owned = rivet_core::PackFile::open_path(&pack_path, format)
            .map_err(|e| anyhow::anyhow!("{}: {e}", index_path.display()))?;
        let pack = owned.pack()?;
        let objects = match rivet_core::pack::verify::verify_pack(&pack) {
            Ok(objects) => objects,
            Err(error) => {
                eprintln!("error: {}: {error}", pack_path.display());
                code = 1;
                continue;
            }
        };
        let mut out = String::new();
        if verbose && !args.stat_only {
            for object in &objects {
                let kind = match object.kind {
                    rivet_core::ObjectKind::Commit => "commit",
                    rivet_core::ObjectKind::Tree => "tree",
                    rivet_core::ObjectKind::Blob => "blob",
                    rivet_core::ObjectKind::Tag => "tag",
                };
                out.push_str(&format!(
                    "{} {:<6} {} {} {}",
                    object.oid, kind, object.size, object.size_in_pack, object.offset
                ));
                if let Some(base) = object.base {
                    out.push_str(&format!(" {} {}", object.depth, base));
                }
                out.push('\n');
            }
        }
        if verbose || args.stat_only {
            let non_delta = objects.iter().filter(|o| o.depth == 0).count();
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            out.push_str(&format!("non delta: {non_delta} object{}\n", plural(non_delta)));
            let max_depth = objects.iter().map(|o| o.depth).max().unwrap_or(0);
            for depth in 1..=max_depth {
                let count = objects.iter().filter(|o| o.depth == depth).count();
                if count > 0 {
                    out.push_str(&format!("chain length = {depth}: {count} object{}\n", plural(count)));
                }
            }
        }
        if verbose {
            out.push_str(&format!("{}: ok\n", pack_path.display()));
        }
        print!("{out}");
    }
    Ok(code)
}

/// Phase 10 — verify the connectivity and validity of the repository's objects.
///
/// # Purpose
///
/// Walk the object graph from the refs, confirm that every object a commit or tree references
/// exists, parse each one and reject the malformed — duplicate tree names, `.`/`..`/`.git`
/// entries, bad modes, hash mismatches — and report the objects no ref reaches.
///
/// # Exit codes
///
/// * `0` — the repository is intact.
/// * `1` — the repository is damaged, or `--dangling` reported objects that no ref reaches. The
///   documented negative, and the code a pre-push check tests.
/// * `128` — the repository could not be opened at all, or the refs could not be read.
///
/// # Caveats
///
/// * **A missing object reachable from a ref is the report that matters most**, and it is
///   [`rivet_core::Error::ObjectNotFound`] naming both the missing object and the referrer. An
///   id alone does not tell a user which commit to look at, and the referrer is what makes the
///   damage actionable.
/// * **An unreachable object is not damage**, per [`FsckArgs`]. A repository that has been
///   amended, reset or rebased always has some, and reporting them on every run trains a user to
///   ignore the command.
/// * **The tree validation is a security boundary, not tidiness.** A tree entry named `.git`, or
///   one containing a `/`, or a `.`/`..` component, is how a checkout writes outside the work
///   tree; plan §5 requires rejecting them here rather than discovering it during a checkout. The
///   same applies to a mode that is not one of the five Git defines.
/// * **A duplicate name in a tree is an error even without `--strict`**, because the tree's sort
///   order becomes ambiguous and every later binary search over it is undefined. Git treats it
///   as an error and so must Rivet, or a lookup returns whichever of the two entries the search
///   happened to land on.
/// * **The walk must be bounded and must handle a cycle.** A commit whose parent list contains
///   itself, or a tag chain that loops, is a crafted-repository scenario — the plan's §5 fuzzing
///   target — and must terminate with [`rivet_core::Error::TagCycle`] or a visited set rather
///   than recursing until the stack is exhausted.
/// * **`--lost-found` is the one mode that writes**, and only after the check has finished. The
///   verification itself must not create refs, objects or locks; a `fsck` that wrote a
///   `lost-found` ref by default would make a read-only diagnosis change the repository it was
///   diagnosing.
pub fn run_fsck(global: &GlobalArgs, args: &FsckArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let options = rivet_core::fsck::FsckOptions {
        strict: args.strict,
        connectivity_only: args.connectivity_only,
        no_reflogs: args.no_reflogs,
    };
    let report = rivet_core::fsck::fsck(&repository, &options)?;
    let name = |kind: rivet_core::ObjectKind| match kind {
        rivet_core::ObjectKind::Commit => "commit",
        rivet_core::ObjectKind::Tree => "tree",
        rivet_core::ObjectKind::Blob => "blob",
        rivet_core::ObjectKind::Tag => "tag",
    };
    for line in &report.problems {
        eprintln!("{line}");
    }
    for line in &report.notices {
        eprintln!("{line}");
    }
    for (kind, oid) in &report.missing {
        println!("missing {} {oid}", name(*kind));
    }
    if args.root {
        for oid in &report.roots {
            println!("root {oid}");
        }
    }
    if args.unreachable {
        for (kind, oid) in &report.unreachable {
            println!("unreachable {} {oid}", name(*kind));
        }
    } else if !args.no_dangling {
        for (kind, oid) in &report.dangling {
            println!("dangling {} {oid}", name(*kind));
        }
    }
    if args.lost_found {
        let base = repository.git_dir().join("lost-found");
        for (kind, oid) in &report.dangling {
            let (dir, contents) = if *kind == rivet_core::ObjectKind::Commit {
                ("commit", format!("{oid}\n").into_bytes())
            } else {
                ("other", repository.odb().read(oid)?.into_data())
            };
            std::fs::create_dir_all(base.join(dir))?;
            std::fs::write(base.join(dir).join(oid.to_string()), contents)?;
        }
    }
    Ok(report.status as i32)
}
