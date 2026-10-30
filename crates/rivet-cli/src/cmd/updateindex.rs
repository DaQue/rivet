//! `update-index`: register working tree content in the index (`--add`, `--remove`, `--refresh`,
//! `--cacheinfo`, `--index-info`, `--stdin`, `--chmod`, the assume-unchanged and skip-worktree
//! bits) and the index-format options (`--index-version`, `--show-index-version`,
//! `--[no-]split-index`). What is not implemented is refused by name.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::index::{
    Index, IndexEntry, IndexEntryExtendedFlags, IndexEntryFlags, IndexExt, IndexVersion, SplitMode,
    Stage,
};
use rivet_core::object::{EntryMode, hash_object};
use rivet_core::worktree::StatData;
use rivet_core::{ObjectFormat, ObjectId, ObjectKind, Repository};

use crate::cli::GlobalArgs;

const USAGE: &str = include_str!("updateindex_usage.txt");

/// Arguments of `update-index`, in git's own grammar.
#[derive(Debug, clap::Args)]
pub struct UpdateIndexArgs {
    /// `[options] [--] [<file>...]`, parsed here in git's order: an option applies to the paths
    /// that follow it.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

/// Every long option of git's `update-index` that this command does not implement.
const REFUSED: [&str; 13] = [
    "again",
    "untracked-cache",
    "no-untracked-cache",
    "test-untracked-cache",
    "force-untracked-cache",
    "fsmonitor",
    "fsmonitor-valid",
    "no-fsmonitor",
    "no-fsmonitor-valid",
    "no-test-untracked-cache",
    "no-force-untracked-cache",
    "no-unresolve",
    "no-again",
];

/// A tri-state mark option (`--assume-unchanged` / `--no-assume-unchanged`).
type Mark = Option<bool>;

/// Why a step stopped the command: the exit status to leave with (the diagnostic is already out).
type Exit = i32;

/// The state `update-index` threads through its options and paths.
struct Session<'a> {
    repository: &'a Repository,
    index: Index,
    root: Option<PathBuf>,
    prefix: Vec<u8>,
    format: ObjectFormat,
    allow_add: bool,
    allow_remove: bool,
    allow_replace: bool,
    info_only: bool,
    force_remove: bool,
    verbose: bool,
    ignore_skip_worktree: bool,
    quiet: bool,
    unmerged: bool,
    ignore_submodules: bool,
    ignore_missing: bool,
    mark_valid: Mark,
    mark_skip: Mark,
    chmod: Option<char>,
    nul: bool,
    has_errors: bool,
    changed: bool,
}

/// What `lstat` said about a path.
enum Lstat {
    Missing,
    Found(StatData),
}

/// `update-index` handler.
///
/// Exit codes: `0`, `1` when `--refresh` found files that need an update, `128` for a failed
/// operation, `129` for a usage error.
pub fn run_update_index(global: &GlobalArgs, args: &UpdateIndexArgs) -> Result<i32> {
    // clap folds the global `-q`/`--verbose` out of the argument list wherever they stand, but
    // for `update-index` their position decides which paths they apply to: read the words as
    // they were typed.
    let raw: Vec<String> = std::env::args().collect();
    let words = match raw.iter().position(|word| word == "update-index") {
        Some(at) => raw[at + 1..].to_vec(),
        None => args.args.clone(),
    };
    match run(global, &words) {
        Ok(code) | Err(code) => Ok(code),
    }
}

fn run(global: &GlobalArgs, args: &[String]) -> std::result::Result<i32, Exit> {
    // Options that need no repository are answered before it is opened, as git does for `-h`.
    if args.iter().take_while(|a| *a != "--").any(|a| a == "-h") {
        print!("{USAGE}");
        return Ok(0);
    }
    let mut repository = super::open(global).map_err(|e| {
        eprintln!("fatal: {e}");
        128
    })?;
    let original_version = repository.index().version();
    let had_index_file = repository.git_dir().join("index").exists();
    let root = repository
        .worktree()
        .ok()
        .and_then(|w| w.root().map(Path::to_path_buf))
        .map(|r| std::fs::canonicalize(&r).unwrap_or(r));
    let prefix = root
        .as_ref()
        .and_then(|root| {
            let cwd = std::env::current_dir().ok()?;
            let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
            let relative = cwd.strip_prefix(root).ok()?;
            let mut bytes = relative.as_os_str().as_bytes().to_vec();
            if !bytes.is_empty() {
                bytes.push(b'/');
            }
            Some(bytes)
        })
        .unwrap_or_default();
    let mut s = Session {
        repository: &repository,
        index: repository.index().clone(),
        root,
        prefix,
        format: repository.format(),
        allow_add: false,
        allow_remove: false,
        allow_replace: false,
        info_only: false,
        force_remove: false,
        verbose: false,
        ignore_skip_worktree: false,
        quiet: false,
        unmerged: false,
        ignore_submodules: false,
        ignore_missing: false,
        mark_valid: None,
        mark_skip: None,
        chmod: None,
        nul: false,
        has_errors: false,
        changed: false,
    };

    let mut show = false;
    let mut version: Option<u32> = None;
    let mut split: Option<bool> = None;
    let mut force_write = false;
    let mut read_stdin = false;
    let mut only_paths = false;
    let mut i = 0;
    while i < args.len() {
        let word = args[i].as_str();
        i += 1;
        if only_paths || word == "-" || !word.starts_with('-') {
            s.path_argument(word)?;
            continue;
        }
        if word == "--" {
            only_paths = true;
            continue;
        }
        let Some(long) = word.strip_prefix("--") else {
            match word {
                "-q" => s.quiet = true,
                "-z" => s.nul = true,
                "-g" => {
                    eprintln!("fatal: update-index: --again is not implemented");
                    return Err(128);
                }
                _ => {
                    let c = word[1..].chars().next().unwrap_or('-');
                    eprintln!("error: unknown switch `{c}'");
                    eprint!("{USAGE}");
                    return Err(129);
                }
            }
            continue;
        };
        let (name, inline) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        let (negated, base) = match name.strip_prefix("no-") {
            Some(base) => (true, base),
            None => (false, name),
        };
        match base {
            "add" => s.allow_add = !negated,
            "replace" => s.allow_replace = !negated,
            "remove" => s.allow_remove = !negated,
            "unmerged" => s.unmerged = !negated,
            "info-only" => s.info_only = !negated,
            "force-remove" => s.force_remove = !negated,
            "verbose" => s.verbose = !negated,
            "ignore-missing" => s.ignore_missing = !negated,
            "ignore-submodules" => s.ignore_submodules = !negated,
            "ignore-skip-worktree-entries" => s.ignore_skip_worktree = !negated,
            "force-write-index" => force_write = !negated,
            "show-index-version" => show = !negated,
            "split-index" => split = Some(!negated),
            "assume-unchanged" => s.mark_valid = if negated { Some(false) } else { Some(true) },
            "skip-worktree" => s.mark_skip = if negated { Some(false) } else { Some(true) },
            "stdin" if !negated => read_stdin = true,
            "unresolve" if !negated => {
                // Every word after the option is a path to put back into its conflicted state.
                s.require_root()?;
                while i < args.len() {
                    let path = s.prefix_path(args[i].as_bytes())?;
                    i += 1;
                    if s.index.unmerge_path(BStr::new(&path), s.format) {
                        s.touch();
                    }
                }
            }
            "refresh" | "really-refresh" if !negated => {
                s.refresh(base == "really-refresh")?;
            }
            "index-info" if !negated => {
                s.allow_add = true;
                s.allow_replace = true;
                s.allow_remove = true;
                s.read_index_info()?;
            }
            "clear-resolve-undo" if !negated => {
                s.index.extensions_mut().retain(|e| !e.is_reuc());
                s.changed = true;
            }
            "chmod" if !negated => {
                let value = match inline {
                    Some(value) => Some(value.to_string()),
                    None => {
                        i += 1;
                        args.get(i - 1).cloned()
                    }
                };
                let Some(value) = value else {
                    eprintln!("error: option `chmod' requires a value");
                    return Err(129);
                };
                s.chmod = match value.as_str() {
                    "+x" => Some('+'),
                    "-x" => Some('-'),
                    _ => {
                        eprintln!("error: option 'chmod' expects \"+x\" or \"-x\"");
                        return Err(129);
                    }
                };
            }
            "cacheinfo" if !negated => {
                if inline.is_some() {
                    eprintln!("error: option `cacheinfo' takes no value");
                    return Err(129);
                }
                i = s.cacheinfo_option(args, i)?;
            }
            "index-version" if !negated => {
                let value = match inline {
                    Some(value) => Some(value.to_string()),
                    None => {
                        i += 1;
                        args.get(i - 1).cloned()
                    }
                };
                let Some(value) = value else {
                    eprintln!("error: option `index-version' requires a value");
                    return Err(129);
                };
                let Ok(number) = value.parse::<u32>() else {
                    eprintln!(
                        "error: option `index-version' expects an integer value with an optional k/m/g suffix"
                    );
                    return Err(129);
                };
                if !(2..=4).contains(&number) {
                    eprintln!("fatal: index-version {number} not in range: 2..4");
                    return Err(128);
                }
                version = Some(number);
            }
            "index-version" => version = None,
            _ => {
                if REFUSED.contains(&name) {
                    eprintln!("fatal: update-index: --{name} is not implemented");
                    return Err(128);
                }
                eprintln!("error: unknown option '{name}'");
                eprint!("{USAGE}");
                return Err(129);
            }
        }
    }

    if read_stdin {
        s.read_paths_from_stdin()?;
    }

    if show {
        // The version the file had when it was read; a missing file reports 0.
        let shown = if had_index_file {
            original_version.as_u32()
        } else {
            0
        };
        println!("{shown}");
    }
    let exit = i32::from(s.has_errors);
    let Session {
        index: mut new_index,
        changed,
        ..
    } = s;
    if version.is_none() && split.is_none() {
        if changed || force_write {
            new_index.sort();
            *repository.index_mut() = new_index;
            repository.write_index().map_err(|e| {
                eprintln!("fatal: {e}");
                128
            })?;
        }
        return Ok(exit);
    }

    let configured = repository
        .config_value(bstr::BStr::new("core.splitindex"))
        .ok()
        .flatten()
        .and_then(|v| rivet_core::config::parse_bool(bstr::BStr::new(&v[..])).ok());
    match (split, configured) {
        (Some(true), Some(false)) => eprintln!(
            "warning: core.splitIndex is set to false; remove or change it, if you really want to enable split index"
        ),
        (Some(false), Some(true)) => eprintln!(
            "warning: core.splitIndex is set to true; remove or change it, if you really want to disable split index"
        ),
        _ => {}
    }
    new_index.sort();
    if let Some(number) = version {
        new_index.set_version(IndexVersion::from_u32(number).map_err(|e| {
            eprintln!("fatal: {e}");
            128
        })?);
    }
    // `core.splitIndex=false` removes a split index that was already there at write time; one
    // this very command creates survives.
    let already_split = new_index
        .extensions()
        .iter()
        .any(|e| e.is_split_index_link());
    let mode = match split {
        Some(true) if configured == Some(false) && already_split => SplitMode::Disable,
        Some(true) => SplitMode::Enable,
        Some(false) => SplitMode::Disable,
        None => SplitMode::Keep,
    };
    let path = repository.git_dir().join("index");
    new_index
        .write_with(&path, repository.format(), mode, 20)
        .map_err(|e| {
            eprintln!("fatal: {e}");
            128
        })?;
    Ok(exit)
}

/// Print a `fatal:` line and give the exit status git uses for it.
fn die(message: impl std::fmt::Display) -> Exit {
    eprintln!("fatal: {message}");
    128
}

/// Git's `verify_path`: a name the index may hold.
fn verify_path(path: &[u8]) -> bool {
    if path.is_empty() || path[0] == b'/' {
        return false;
    }
    path.split(|b| *b == b'/').all(|component| {
        !component.is_empty()
            && component != b"."
            && component != b".."
            && !component.eq_ignore_ascii_case(b".git")
    })
}

/// Git's `create_ce_mode`: the index mode for whatever mode bits were given.
fn create_ce_mode(mode: u32) -> EntryMode {
    match mode & 0o170000 {
        0o120000 => EntryMode::Link,
        0o040000 | 0o160000 => EntryMode::Commit,
        _ if mode & 0o100 != 0 => EntryMode::BlobExecutable,
        _ => EntryMode::Blob,
    }
}

/// Whether two modes are the same as far as a refresh is concerned: the file type always counts,
/// the executable bit only when `core.filemode` is on.
fn same_mode(entry: EntryMode, found: EntryMode, filemode: bool) -> bool {
    let class = |mode: EntryMode| match mode {
        EntryMode::Blob | EntryMode::BlobExecutable => 0,
        EntryMode::Link => 1,
        EntryMode::Tree | EntryMode::Commit => 2,
    };
    class(entry) == class(found)
        && (!filemode
            || class(entry) != 0
            || (entry == EntryMode::BlobExecutable) == (found == EntryMode::BlobExecutable))
}

/// The octal number at the start of `text` and what follows it.
fn octal_prefix(text: &[u8]) -> Option<(u32, &[u8])> {
    let digits = text
        .iter()
        .take_while(|b| (b'0'..=b'7').contains(b))
        .count();
    if digits == 0 {
        return None;
    }
    let mut value = 0u64;
    for b in &text[..digits] {
        value = value * 8 + u64::from(b - b'0');
        if value > u64::from(u32::MAX) {
            return None;
        }
    }
    Some((value as u32, &text[digits..]))
}

/// The id at the start of `text` (exactly the hash width in hex digits) and what follows it.
fn hex_prefix(format: ObjectFormat, text: &[u8]) -> Option<(ObjectId, &[u8])> {
    let width = format.len() * 2;
    let head = text.get(..width)?;
    ObjectId::from_hex(format, BStr::new(head))
        .ok()
        .map(|oid| (oid, &text[width..]))
}

/// The bytes inside a leading C-style quoted string; `None` for a malformed or unterminated one
/// and for text after the closing quote.
fn unquote(text: &[u8]) -> Option<Vec<u8>> {
    let inner = text.strip_prefix(b"\"")?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        match inner[i] {
            b'"' => return (i + 1 == inner.len()).then_some(out),
            b'\\' => {
                i += 1;
                let escaped = *inner.get(i)?;
                match escaped {
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'v' => out.push(11),
                    b'\\' | b'"' => out.push(escaped),
                    b'0'..=b'3' => {
                        let digits = inner.get(i..i + 3)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return None;
                        }
                        let value = digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
                        out.push(u8::try_from(value).ok()?);
                        i += 2;
                    }
                    _ => return None,
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    None
}

/// Whether the blob the index holds for `path` has a CR in it (what `text=auto` asks).
fn index_blob_has_cr(index: &Index, repository: &Repository, path: &BStr) -> bool {
    let Some(entry) = index.entry(path) else {
        return false;
    };
    if entry.stage() != Stage::Normal || matches!(entry.mode, EntryMode::Link | EntryMode::Commit) {
        return false;
    }
    repository
        .odb()
        .read_typed(&entry.oid, ObjectKind::Blob)
        .map(|object| object.data().contains(&b'\r'))
        .unwrap_or(false)
}

impl Session<'_> {
    fn report(&self, what: &str, path: &[u8]) {
        if self.verbose {
            println!("{what} '{}'", path.as_bstr());
        }
    }

    /// Something in the index changed: it must be written, and no cached tree survives.
    fn touch(&mut self) {
        self.changed = true;
        self.index.extensions_mut().retain(|e| !e.is_tree());
    }

    fn require_root(&self) -> std::result::Result<&Path, Exit> {
        self.root
            .as_deref()
            .ok_or_else(|| die("this operation must be run in a work tree"))
    }

    /// A path from the command line (or standard input): update it, then apply `--chmod`.
    fn path_argument(&mut self, arg: &str) -> std::result::Result<(), Exit> {
        self.require_root()?;
        let path = self.prefix_path(arg.as_bytes())?;
        self.update_path(&path)
    }

    fn update_path(&mut self, path: &[u8]) -> std::result::Result<(), Exit> {
        self.update_one(path)?;
        if let Some(flip) = self.chmod {
            self.chmod_path(flip, path)?;
        }
        Ok(())
    }

    /// Git's `prefix_path`: the path relative to the work tree root, `.`/`..` resolved.
    fn prefix_path(&self, orig: &[u8]) -> std::result::Result<Vec<u8>, Exit> {
        let root = self.require_root()?;
        let root_bytes = root.as_os_str().as_bytes();
        let outside = || {
            die(format!(
                "'{}' is outside repository at '{}'",
                orig.as_bstr(),
                root.display()
            ))
        };
        let mut joined: Vec<u8>;
        if orig.first() == Some(&b'/') {
            let rest = orig.strip_prefix(root_bytes).ok_or_else(outside)?;
            if !rest.is_empty() && rest[0] != b'/' {
                return Err(outside());
            }
            joined = rest.to_vec();
        } else {
            joined = self.prefix.clone();
            joined.extend_from_slice(orig);
        }
        let trailing = joined.last() == Some(&b'/');
        let mut parts: Vec<&[u8]> = Vec::new();
        for part in joined.split(|b| *b == b'/') {
            match part {
                b"" | b"." => {}
                b".." => {
                    parts.pop().ok_or_else(outside)?;
                }
                other => parts.push(other),
            }
        }
        let mut out = parts.join(&b'/');
        if trailing && !out.is_empty() {
            out.push(b'/');
        }
        Ok(out)
    }

    fn find(&self, path: &[u8], stage: Stage) -> Option<usize> {
        self.index
            .entries()
            .iter()
            .position(|e| e.path == path && e.stage() == stage)
    }

    fn has_symlink_leading_path(&self, path: &[u8]) -> bool {
        let Some(root) = self.root.as_deref() else {
            return false;
        };
        let mut end = 0;
        while let Some(at) = path[end..].iter().position(|b| *b == b'/') {
            end += at;
            let leading = root.join(std::ffi::OsStr::from_bytes(&path[..end]));
            if std::fs::symlink_metadata(&leading).is_ok_and(|m| m.file_type().is_symlink()) {
                return true;
            }
            end += 1;
        }
        false
    }

    fn lstat(&self, path: &[u8]) -> std::result::Result<Lstat, ()> {
        let root = self.root.as_deref().ok_or(())?;
        let worktree = self.repository.worktree().map_err(|_| ())?;
        let full = root.join(std::ffi::OsStr::from_bytes(path));
        match std::fs::symlink_metadata(&full) {
            Ok(meta) => StatData::from_metadata(&meta, worktree.filemode(), worktree.symlinks())
                .map(Lstat::Found)
                .map_err(|_| ()),
            // ENOENT, and ENOTDIR for a path below a regular file: both mean "not there".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
                Ok(Lstat::Missing)
            }
            Err(e) => {
                eprintln!("error: lstat(\"{}\"): {e}", path.as_bstr());
                Err(())
            }
        }
    }

    fn gitlink_head(&self, path: &[u8]) -> Option<ObjectId> {
        let dir = self.root.as_deref()?.join(std::ffi::OsStr::from_bytes(
            path.trim_end_with(|c| c == '/'),
        ));
        if !dir.join(".git").exists() {
            return None;
        }
        Repository::open_or_discover(&dir).ok()?.head().ok()?.oid()
    }

    /// Git's `update_one`.
    fn update_one(&mut self, path: &[u8]) -> std::result::Result<(), Exit> {
        if !verify_path(path) {
            eprintln!("Ignoring path {}", path.as_bstr());
            return Ok(());
        }
        if self.ignore_skip_worktree
            && let Some(i) = self.find(path, Stage::Normal)
            && self.index.entries()[i].skip_worktree()
        {
            return Ok(());
        }
        if let Some(on) = self.mark_valid {
            return self.mark_flag(path, on, true);
        }
        if let Some(on) = self.mark_skip {
            return self.mark_flag(path, on, false);
        }
        if self.force_remove {
            if !self.index.remove_path(BStr::new(path)).is_empty() {
                self.touch();
            }
            self.report("remove", path);
            return Ok(());
        }
        if self.process_path(path).is_err() {
            return Err(die(format!("Unable to process path {}", path.as_bstr())));
        }
        self.report("add", path);
        Ok(())
    }

    /// `--[no-]assume-unchanged` and `--[no-]skip-worktree` on one path.
    fn mark_flag(&mut self, path: &[u8], on: bool, valid: bool) -> std::result::Result<(), Exit> {
        let Some(i) = self.find(path, Stage::Normal) else {
            return Err(die(format!("Unable to mark file {}", path.as_bstr())));
        };
        let entry = &mut self.index.entries_mut()[i];
        if valid {
            entry.flags = entry.flags.set_assume_valid(on);
        } else {
            let flags =
                IndexEntryExtendedFlags::from_bits(entry.extended_flags).set_skip_worktree(on);
            entry.extended_flags = flags.bits();
            entry.flags =
                IndexEntryFlags::from_bits(entry.flags.bits()).set_extended(flags.bits() != 0);
        }
        self.touch();
        Ok(())
    }

    /// Git's `chmod_path`: `--chmod=+x|-x` on a tracked regular file.
    fn chmod_path(&mut self, flip: char, path: &[u8]) -> std::result::Result<(), Exit> {
        let fail = || {
            die(format!(
                "git update-index: cannot chmod {flip}x '{}'",
                path.as_bstr()
            ))
        };
        let i = self.find(path, Stage::Normal).ok_or_else(fail)?;
        let entry = &mut self.index.entries_mut()[i];
        if !matches!(entry.mode, EntryMode::Blob | EntryMode::BlobExecutable) {
            return Err(fail());
        }
        entry.mode = if flip == '+' {
            EntryMode::BlobExecutable
        } else {
            EntryMode::Blob
        };
        self.touch();
        self.report(&format!("chmod {flip}x"), path);
        Ok(())
    }

    /// Git's `process_path`.
    fn process_path(&mut self, path: &[u8]) -> std::result::Result<(), ()> {
        if self.has_symlink_leading_path(path) {
            eprintln!("error: '{}' is beyond a symbolic link", path.as_bstr());
            return Err(());
        }
        match self.lstat(path)? {
            Lstat::Missing => self.remove_one_path(path),
            Lstat::Found(sd) if sd.mode == EntryMode::Tree => self.process_directory(path, sd),
            Lstat::Found(sd) => self.add_one_path(path, &sd, None),
        }
    }

    fn remove_one_path(&mut self, path: &[u8]) -> std::result::Result<(), ()> {
        if !self.allow_remove {
            eprintln!(
                "error: {}: does not exist and --remove not passed",
                path.as_bstr()
            );
            return Err(());
        }
        if !self.index.remove_path(BStr::new(path)).is_empty() {
            self.touch();
        }
        Ok(())
    }

    /// Git's `process_directory`: only a submodule's directory can be added.
    fn process_directory(&mut self, path: &[u8], sd: StatData) -> std::result::Result<(), ()> {
        let name = path.trim_end_with(|c| c == '/');
        if let Some(i) = self.find(name, Stage::Normal) {
            if self.index.entries()[i].is_gitlink() {
                return match self.gitlink_head(name) {
                    None => Ok(()),
                    Some(head) => self.add_one_path(name, &sd, Some(head)),
                };
            }
            return self.remove_one_path(name);
        }
        let mut below = name.to_vec();
        below.push(b'/');
        if self
            .index
            .entries()
            .iter()
            .any(|e| e.path.starts_with(&below))
        {
            eprintln!(
                "error: {}: is a directory - add individual files instead",
                name.as_bstr()
            );
            return Err(());
        }
        if let Some(head) = self.gitlink_head(name) {
            return self.add_one_path(name, &sd, Some(head));
        }
        eprintln!(
            "error: {}: is a directory - add files inside instead",
            name.as_bstr()
        );
        Err(())
    }

    /// Git's `add_one_path`: hash the file (or take a submodule's `HEAD`) and register it.
    fn add_one_path(
        &mut self,
        path: &[u8],
        sd: &StatData,
        gitlink: Option<ObjectId>,
    ) -> std::result::Result<(), ()> {
        let worktree = self.repository.worktree().map_err(|_| ())?;
        let name = BStr::new(path);
        let (mode, oid) = match gitlink {
            Some(head) => (EntryMode::Commit, head),
            None => {
                let mode = if !worktree.filemode() && sd.mode == EntryMode::BlobExecutable {
                    EntryMode::Blob
                } else {
                    sd.mode
                };
                let oid = self.hash_path(name, sd).map_err(|e| {
                    eprintln!("error: {e}");
                })?;
                (mode, oid)
            }
        };
        let entry = IndexEntry {
            ctime_seconds: sd.ctime_seconds,
            ctime_nanos: sd.ctime_nanos,
            mtime_seconds: sd.mtime_seconds,
            mtime_nanos: sd.mtime_nanos,
            dev: sd.dev,
            ino: sd.ino,
            mode,
            uid: sd.uid,
            gid: sd.gid,
            size: sd.size,
            oid,
            flags: IndexEntryFlags::with(Stage::Normal, path.len()),
            extended_flags: 0,
            path: BString::from(path),
        };
        if self.add_cache_entry(entry).is_err() {
            eprintln!(
                "error: {}: cannot add to the index - missing --add option?",
                path.as_bstr()
            );
            return Err(());
        }
        Ok(())
    }

    fn hash_path(&self, path: &BStr, sd: &StatData) -> rivet_core::Result<ObjectId> {
        let worktree = self.repository.worktree()?;
        let mut data = worktree.read_file(path)?;
        if sd.mode != EntryMode::Link {
            data = worktree.clean_content(
                path,
                data,
                &mut || index_blob_has_cr(&self.index, self.repository, path),
                !self.info_only,
            )?;
        }
        if self.info_only {
            Ok(hash_object(self.format, ObjectKind::Blob, &data)?.0)
        } else {
            self.repository.odb().write_raw(ObjectKind::Blob, &data)
        }
    }

    /// Git's `add_cache_entry` with the `--add` / `--replace` options: replaces an entry of the same
    /// path and stage, refuses a new path without `--add`, and resolves file/directory clashes only
    /// with `--replace`.
    fn add_cache_entry(&mut self, entry: IndexEntry) -> std::result::Result<(), ()> {
        let stage = entry.stage();
        if self.find(&entry.path, stage).is_some() {
            self.index.insert(entry);
            self.changed = true;
            return Ok(());
        }
        let mut ok_to_add = self.allow_add;
        if stage == Stage::Normal && !self.index.remove_path(entry.path.as_ref()).is_empty() {
            // A merged entry replaces whatever conflict stages the path had.
            ok_to_add = true;
        }
        if !ok_to_add {
            return Err(());
        }
        if !verify_path(&entry.path) {
            eprintln!("error: invalid path '{}'", entry.path);
            return Err(());
        }
        // A file where a directory is (or the reverse) needs `--replace`.
        let mut clashing: Vec<BString> = Vec::new();
        let mut end = 0;
        while let Some(at) = entry.path[end..].iter().position(|b| *b == b'/') {
            end += at;
            let dir = &entry.path[..end];
            if self.index.entries().iter().any(|e| e.path == dir) {
                clashing.push(BString::from(dir));
            }
            end += 1;
        }
        let mut below = entry.path.to_vec();
        below.push(b'/');
        for e in self.index.entries() {
            if e.path.starts_with(&below) {
                clashing.push(e.path.clone());
            }
        }
        if !clashing.is_empty() {
            if !self.allow_replace {
                eprintln!(
                    "error: '{}' appears as both a file and as a directory",
                    entry.path
                );
                return Err(());
            }
            self.index
                .entries_mut()
                .retain(|e| !clashing.contains(&e.path));
        }
        self.index.insert(entry);
        self.changed = true;
        Ok(())
    }

    /// Git's `add_cacheinfo`: an entry with no stat data and no object lookup.
    fn add_cacheinfo(
        &mut self,
        mode: u32,
        oid: ObjectId,
        path: &[u8],
        stage: u8,
    ) -> std::result::Result<(), ()> {
        if !verify_path(path) {
            eprintln!("error: Invalid path '{}'", path.as_bstr());
            return Err(());
        }
        if mode & 0o170000 == 0o040000 {
            eprintln!(
                "error: update-index: sparse directory entries (mode 040000) are not implemented"
            );
            return Err(());
        }
        let stage = Stage::from_bits(stage).map_err(|_| ())?;
        let entry = IndexEntry {
            ctime_seconds: 0,
            ctime_nanos: 0,
            mtime_seconds: 0,
            mtime_nanos: 0,
            dev: 0,
            ino: 0,
            mode: create_ce_mode(mode),
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: IndexEntryFlags::with(stage, path.len()),
            extended_flags: 0,
            path: BString::from(path),
        };
        if self.add_cache_entry(entry).is_err() {
            eprintln!(
                "error: {}: cannot add to the index - missing --add option?",
                path.as_bstr()
            );
            return Err(());
        }
        self.report("add", path);
        Ok(())
    }

    /// `--cacheinfo` with the words after it; returns the index of the first word not consumed.
    fn cacheinfo_option(&mut self, args: &[String], at: usize) -> std::result::Result<usize, Exit> {
        let rest = &args[at..];
        if let Some(word) = rest.first() {
            let bytes = word.as_bytes();
            if let Some((mode, tail)) = octal_prefix(bytes)
                && let Some(tail) = tail.strip_prefix(b",")
                && let Some((oid, tail)) = hex_prefix(self.format, tail)
                && let Some(path) = tail.strip_prefix(b",")
            {
                return match self.add_cacheinfo(mode, oid, path, 0) {
                    Ok(()) => Ok(at + 1),
                    Err(()) => Err(die(format!(
                        "git update-index: --cacheinfo cannot add {}",
                        path.as_bstr()
                    ))),
                };
            }
        }
        if rest.len() < 3 {
            eprintln!("error: option 'cacheinfo' expects <mode>,<sha1>,<path>");
            return Err(129);
        }
        let cannot = |word: &str| die(format!("git update-index: --cacheinfo cannot add {word}"));
        let Some((mode, tail)) = octal_prefix(rest[0].as_bytes()) else {
            return Err(cannot(&rest[0]));
        };
        if !tail.is_empty() {
            return Err(cannot(&rest[0]));
        }
        let Some((oid, _)) = hex_prefix(self.format, rest[1].as_bytes()) else {
            return Err(cannot(&rest[1]));
        };
        if self
            .add_cacheinfo(mode, oid, rest[2].as_bytes(), 0)
            .is_err()
        {
            return Err(cannot(&rest[2]));
        }
        Ok(at + 3)
    }

    /// The lines of standard input, split on LF (CRLF too) or on NUL with `-z`.
    fn stdin_lines(&self) -> Vec<Vec<u8>> {
        let mut data = Vec::new();
        let _ = std::io::stdin().lock().read_to_end(&mut data);
        let separator = if self.nul { 0 } else { b'\n' };
        let mut lines: Vec<Vec<u8>> = data
            .split(|b| *b == separator)
            .map(<[u8]>::to_vec)
            .collect();
        if lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        if !self.nul {
            for line in &mut lines {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
        }
        lines
    }

    fn read_paths_from_stdin(&mut self) -> std::result::Result<(), Exit> {
        self.require_root()?;
        for line in self.stdin_lines() {
            let line = if !self.nul && line.first() == Some(&b'"') {
                unquote(&line).ok_or_else(|| die("line is badly quoted"))?
            } else {
                line
            };
            let path = self.prefix_path(&line)?;
            self.update_path(&path)?;
        }
        Ok(())
    }

    /// `--index-info`: `mode sha1 TAB path`, `mode type sha1 TAB path` or `mode sha1 stage TAB path`.
    fn read_index_info(&mut self) -> std::result::Result<(), Exit> {
        let hexsz = self.format.len() * 2;
        for line in self.stdin_lines() {
            let bad = || die(format!("malformed index info {}", line.as_bstr()));
            let (mode, after_mode) = octal_prefix(&line).ok_or_else(bad)?;
            if after_mode.first() != Some(&b' ') {
                return Err(bad());
            }
            let tab = after_mode
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(bad)?;
            if tab < hexsz + 1 {
                return Err(bad());
            }
            let (stage, oid_end) =
                if after_mode[tab - 2] == b' ' && (b'0'..=b'3').contains(&after_mode[tab - 1]) {
                    (after_mode[tab - 1] - b'0', tab - 2)
                } else {
                    (0, tab)
                };
            if oid_end < hexsz + 1 || after_mode[oid_end - hexsz - 1] != b' ' {
                return Err(bad());
            }
            let (oid, _) =
                hex_prefix(self.format, &after_mode[oid_end - hexsz..oid_end]).ok_or_else(bad)?;
            let raw_path = &after_mode[tab + 1..];
            let path = if !self.nul && raw_path.first() == Some(&b'"') {
                unquote(raw_path)
                    .ok_or_else(|| die("git update-index: bad quoting of path name"))?
            } else {
                raw_path.to_vec()
            };
            if !verify_path(&path) {
                eprintln!("Ignoring path {}", path.as_bstr());
                continue;
            }
            if mode == 0 {
                if !self.index.remove_path(BStr::new(&path)).is_empty() {
                    self.touch();
                }
            } else if self.add_cacheinfo(mode, oid, &path, stage).is_err() {
                return Err(die(format!(
                    "git update-index: unable to update {}",
                    path.as_bstr()
                )));
            }
        }
        Ok(())
    }

    /// `--refresh` / `--really-refresh`: bring the cached stat data up to date, and name every file
    /// whose content really differs.
    fn refresh(&mut self, really: bool) -> std::result::Result<(), Exit> {
        self.require_root()?;
        let worktree = self.repository.worktree().map_err(die)?;
        let index_mtime = worktree.index_mtime();
        let filemode = worktree.filemode();
        let mut refreshed: Vec<(usize, StatData)> = Vec::new();
        let mut cleared: Vec<usize> = Vec::new();
        let mut messages: Vec<String> = Vec::new();
        let mut errors = false;
        let entries = self.index.entries();
        let mut i = 0;
        while i < entries.len() {
            let entry = &entries[i];
            let at = i;
            i += 1;
            if self.ignore_submodules && entry.is_gitlink() {
                continue;
            }
            if self.ignore_skip_worktree && entry.skip_worktree() {
                continue;
            }
            if entry.stage() != Stage::Normal {
                while i < entries.len() && entries[i].path == entry.path {
                    i += 1;
                }
                if self.unmerged {
                    continue;
                }
                if !self.quiet {
                    messages.push(format!("{}: needs merge", entry.path));
                }
                errors = true;
                continue;
            }
            let valid = entry.flags.assume_valid();
            if !really && (valid || entry.skip_worktree()) {
                continue;
            }
            let found = if self.has_symlink_leading_path(&entry.path) {
                Lstat::Missing
            } else {
                self.lstat(&entry.path).map_err(|()| 128)?
            };
            let mut present = true;
            let modified = match found {
                Lstat::Missing => {
                    if self.ignore_missing {
                        continue;
                    }
                    present = false;
                    true
                }
                Lstat::Found(sd) => {
                    if entry.is_intent_to_add() {
                        true
                    } else if entry.is_gitlink() {
                        sd.mode != EntryMode::Tree
                            || self
                                .gitlink_head(&entry.path)
                                .is_some_and(|head| head != entry.oid)
                    } else if !same_mode(entry.mode, sd.mode, filemode) {
                        true
                    } else {
                        let stat_same = sd.ctime_seconds == entry.ctime_seconds
                            && sd.ctime_nanos == entry.ctime_nanos
                            && sd.mtime_seconds == entry.mtime_seconds
                            && sd.mtime_nanos == entry.mtime_nanos
                            && sd.dev == entry.dev
                            && sd.ino == entry.ino
                            && sd.uid == entry.uid
                            && sd.gid == entry.gid
                            && sd.size == entry.size;
                        if stat_same && !sd.is_racy(index_mtime) {
                            continue;
                        }
                        // A different size is a different file, unless the entry never saw a
                        // stat (size 0, as after `--cacheinfo`) and has to be compared by content.
                        if sd.size != entry.size && entry.size != 0 {
                            true
                        } else {
                            let hash = worktree.hash_file_with(BStr::new(&entry.path), &mut || {
                                index_blob_has_cr(
                                    &self.index,
                                    self.repository,
                                    BStr::new(&entry.path),
                                )
                            });
                            match hash {
                                Ok(oid) if oid == entry.oid => {
                                    if !stat_same {
                                        refreshed.push((at, sd));
                                    }
                                    continue;
                                }
                                Ok(_) => true,
                                Err(e) => {
                                    eprintln!("error: {e}");
                                    return Err(128);
                                }
                            }
                        }
                    }
                }
            };
            if modified {
                if really && valid && present {
                    cleared.push(at);
                }
                if self.quiet {
                    continue;
                }
                messages.push(format!("{}: needs update", entry.path));
                errors = true;
            }
        }
        for message in messages {
            println!("{message}");
        }
        let changed = !refreshed.is_empty() || !cleared.is_empty();
        let entries = self.index.entries_mut();
        for (at, sd) in refreshed {
            let entry = &mut entries[at];
            entry.ctime_seconds = sd.ctime_seconds;
            entry.ctime_nanos = sd.ctime_nanos;
            entry.mtime_seconds = sd.mtime_seconds;
            entry.mtime_nanos = sd.mtime_nanos;
            entry.dev = sd.dev;
            entry.ino = sd.ino;
            entry.uid = sd.uid;
            entry.gid = sd.gid;
            entry.size = sd.size;
        }
        for at in cleared {
            let entry = &mut entries[at];
            entry.flags = entry.flags.set_assume_valid(false);
        }
        if changed {
            self.changed = true;
        }
        if errors {
            self.has_errors = true;
        }
        Ok(())
    }
}
