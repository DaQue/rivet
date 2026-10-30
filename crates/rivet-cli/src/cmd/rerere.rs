//! `rerere`: reuse recorded resolutions of conflicted merges.
//!
//! The on-disk format is git's: `.git/rr-cache/<hash>/{preimage,postimage}[.<variant>]` plus
//! `.git/MERGE_RR` (`<hash>[.<variant>]\t<path>\0` records). The conflict id is the SHA of the
//! normalised conflict hunks (each side followed by a NUL, the smaller side first), so it does
//! not depend on labels, on the side order or on diff3 base text.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::index::Stage;
use rivet_core::merge::{ConflictStyle, merge_content};
use rivet_core::{ObjectFormat, Repository};

use crate::cli::GlobalArgs;

const USAGE: &str =
    "usage: git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]

    --[no-]rerere-autoupdate
                          register clean resolutions in index

";

/// Arguments of `rerere`, in git's own grammar.
#[derive(Debug, clap::Args)]
pub struct RerereArgs {
    /// `[--[no-]rerere-autoupdate] [clear | forget <pathspec>... | diff | status | remaining | gc]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const PREIMAGE: u8 = 1;
const POSTIMAGE: u8 = 2;
const MARKER_SIZE: usize = 7;

/// One conflict id: the hash and the variant slot under it (`-1` while unassigned).
#[derive(Clone, Debug)]
struct Id {
    hex: String,
    variant: i32,
}

/// How a `rerere` entry point was asked to treat `rerere.autoUpdate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoUpdate {
    /// Whatever `rerere.autoUpdate` says.
    Config,
    /// `--rerere-autoupdate`.
    Yes,
    /// `--no-rerere-autoupdate`.
    No,
}

/// The `--[no-]rerere-autoupdate` flag of the command being run, for [`rerere_hook`].
static COMMAND_FLAG: AtomicU8 = AtomicU8::new(0);

/// Record the `--rerere-autoupdate` (`Some(true)`) / `--no-rerere-autoupdate` (`Some(false)`)
/// flag of the running command; commands that conflict call this once, before they might.
pub fn set_autoupdate_flag(flag: Option<bool>) {
    COMMAND_FLAG.store(
        match flag {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        },
        Ordering::Relaxed,
    );
}

struct Rerere {
    cache: PathBuf,
    merge_rr: PathBuf,
    root: PathBuf,
    /// Sorted by path; `None` is an entry that is resolved or forgotten and not written back.
    rr: BTreeMap<BString, Option<Id>>,
    dirs: HashMap<String, Vec<u8>>,
    autoupdate: bool,
    format: ObjectFormat,
}

/// Is rerere on for this repository? `rerere.enabled` wins; unset means "iff `rr-cache` exists".
fn is_enabled(repository: &Repository) -> bool {
    let cache = repository.common_dir().join("rr-cache");
    match repository
        .config()
        .get_bool(BStr::new("rerere.enabled"))
        .ok()
        .flatten()
    {
        Some(false) => false,
        None => cache.is_dir(),
        Some(true) => {
            if !cache.is_dir() {
                let _ = std::fs::create_dir(&cache);
            }
            true
        }
    }
}

fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

/// Set while the hook runs for an autostash, whose rerere run git does not announce.
static QUIET: AtomicBool = AtomicBool::new(false);

/// [`rerere_hook`] as the stash apply behind `checkout -m`, `switch -m` and `rebase --autostash`
/// runs it: nothing is announced, but the preimages and any replayed resolution are done. Git
/// leaves `MERGE_RR` empty after a rebase and removes it after a checkout (`keep_file`).
pub fn rerere_hook_autostash(repository: &mut Repository, keep_file: bool) -> Result<()> {
    QUIET.store(true, Ordering::Relaxed);
    let result = rerere_hook(repository);
    QUIET.store(false, Ordering::Relaxed);
    if keep_file {
        if is_enabled(repository) {
            let _ = std::fs::write(repository.git_dir().join("MERGE_RR"), b"");
        }
    } else {
        remove_merge_rr(repository.git_dir());
    }
    result
}

/// Remove `MERGE_RR`, the way git's `remove_merge_branch_state` does.
pub fn remove_merge_rr(git_dir: &Path) {
    let _ = std::fs::remove_file(git_dir.join("MERGE_RR"));
}

/// `git rerere` as other commands call it after a conflict and before a commit: record
/// preimages, replay recorded resolutions, record the hand-made ones. A no-op when disabled.
pub fn rerere_hook(repository: &mut Repository) -> Result<()> {
    run_plain(
        repository,
        match COMMAND_FLAG.load(Ordering::Relaxed) {
            1 => AutoUpdate::Yes,
            2 => AutoUpdate::No,
            _ => AutoUpdate::Config,
        },
    )
}

fn run_plain(repository: &mut Repository, autoupdate: AutoUpdate) -> Result<()> {
    let Some(mut state) = Rerere::setup(repository, autoupdate)? else {
        return Ok(());
    };
    state.do_plain(repository)
}

impl Rerere {
    fn setup(repository: &Repository, autoupdate: AutoUpdate) -> Result<Option<Self>> {
        if !is_enabled(repository) {
            return Ok(None);
        }
        let configured = repository
            .config()
            .get_bool(BStr::new("rerere.autoupdate"))
            .ok()
            .flatten()
            .unwrap_or(false);
        let root = repository
            .worktree_path()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
        let mut state = Rerere {
            cache: repository.common_dir().join("rr-cache"),
            merge_rr: repository.git_dir().join("MERGE_RR"),
            root,
            rr: BTreeMap::new(),
            dirs: HashMap::new(),
            autoupdate: match autoupdate {
                AutoUpdate::Config => configured,
                AutoUpdate::Yes => true,
                AutoUpdate::No => false,
            },
            format: repository.format(),
        };
        state.read_rr()?;
        Ok(Some(state))
    }

    fn read_rr(&mut self) -> Result<()> {
        let Ok(data) = std::fs::read(&self.merge_rr) else {
            return Ok(());
        };
        let hexsz = self.format.hex_len();
        for record in data.split(|&b| b == 0) {
            if record.is_empty() {
                continue;
            }
            let corrupt = || anyhow::anyhow!("corrupt MERGE_RR");
            if record.len() < hexsz + 2 || !record[..hexsz].iter().all(u8::is_ascii_hexdigit) {
                return Err(corrupt());
            }
            let hex = String::from_utf8_lossy(&record[..hexsz]).into_owned();
            let rest = &record[hexsz..];
            let (variant, rest) = if rest[0] == b'.' {
                let digits = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                let number = std::str::from_utf8(&rest[1..1 + digits])
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                    .ok_or_else(corrupt)?;
                (number, &rest[1 + digits..])
            } else {
                (0, rest)
            };
            let Some(path) = rest.strip_prefix(b"\t") else {
                return Err(corrupt());
            };
            self.rr
                .insert(BString::from(path), Some(Id { hex, variant }));
        }
        Ok(())
    }

    fn write_rr(&self) -> Result<()> {
        let mut out = Vec::new();
        for (path, id) in &self.rr {
            let Some(id) = id else { continue };
            if id.variant > 0 {
                out.extend_from_slice(format!("{}.{}\t", id.hex, id.variant).as_bytes());
            } else {
                out.extend_from_slice(format!("{}\t", id.hex).as_bytes());
            }
            out.extend_from_slice(path);
            out.push(0);
        }
        std::fs::write(&self.merge_rr, out)
            .map_err(|e| anyhow::anyhow!("could not write '{}': {e}", self.merge_rr.display()))
    }

    // --- the cache -----------------------------------------------------------------------

    /// The variant flags under `hex`, scanned from disk on first use.
    fn status(&mut self, hex: &str) -> &mut Vec<u8> {
        let dir = self.cache.join(hex);
        self.dirs.entry(hex.to_string()).or_insert_with(|| {
            let mut status: Vec<u8> = Vec::new();
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return status;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let (flag, rest) = if let Some(rest) = name.strip_prefix("preimage") {
                    (PREIMAGE, rest)
                } else if let Some(rest) = name.strip_prefix("postimage") {
                    (POSTIMAGE, rest)
                } else {
                    continue;
                };
                let variant = if rest.is_empty() {
                    0
                } else if let Some(n) = rest.strip_prefix('.').and_then(|n| n.parse::<usize>().ok())
                {
                    n
                } else {
                    continue;
                };
                if status.len() <= variant {
                    status.resize(variant + 1, 0);
                }
                status[variant] |= flag;
            }
            status
        })
    }

    fn flags(&mut self, id: &Id) -> u8 {
        let variant = id.variant.max(0) as usize;
        self.status(&id.hex).get(variant).copied().unwrap_or(0)
    }

    fn set_flags(&mut self, id: &Id, flags: u8) {
        let variant = id.variant.max(0) as usize;
        let status = self.status(&id.hex);
        if status.len() <= variant {
            status.resize(variant + 1, 0);
        }
        status[variant] = flags;
    }

    fn dir_path(&self, id: &Id) -> PathBuf {
        self.cache.join(&id.hex)
    }

    fn file_path(&self, id: &Id, file: &str) -> PathBuf {
        if id.variant > 0 {
            self.dir_path(id).join(format!("{file}.{}", id.variant))
        } else {
            self.dir_path(id).join(file)
        }
    }

    fn unlink_item(&mut self, id: &Id) {
        for file in ["thisimage", "preimage", "postimage"] {
            let _ = std::fs::remove_file(self.file_path(id, file));
        }
        self.set_flags(id, 0);
    }

    fn remove_variant(&mut self, id: &Id) {
        for file in ["postimage", "preimage"] {
            let _ = std::fs::remove_file(self.file_path(id, file));
        }
        self.set_flags(id, 0);
    }

    fn assign_variant(&mut self, id: &mut Id) {
        if id.variant < 0 {
            let status = self.status(&id.hex);
            let slot = status.iter().position(|&f| f == 0).unwrap_or(status.len());
            id.variant = slot as i32;
            if status.len() <= slot {
                status.resize(slot + 1, 0);
            }
        }
        let _ = std::fs::create_dir_all(self.dir_path(id));
    }

    // --- conflict parsing ----------------------------------------------------------------

    /// Scan the file at `path` (relative to the work tree). Returns the number of conflict hunks
    /// (`-1` on a parse or read error) and the conflict id when asked for.
    fn handle_file(&self, path: &[u8], want_hash: bool, output: Option<&Path>) -> (i32, String) {
        let full = self.root.join(os(path));
        let data = match std::fs::read(&full) {
            Ok(data) => data,
            Err(e) => {
                eprintln!(
                    "error: could not open '{}': {}",
                    path.as_bstr(),
                    os_error(&e)
                );
                return (-1, String::new());
            }
        };
        self.handle_bytes(&data, path.as_bstr(), want_hash, output)
    }

    fn handle_bytes(
        &self,
        data: &[u8],
        label: &BStr,
        want_hash: bool,
        output: Option<&Path>,
    ) -> (i32, String) {
        let mut out = Vec::new();
        let mut hash_input = Vec::new();
        let hunks = handle_path(
            data,
            MARKER_SIZE,
            &mut out,
            want_hash.then_some(&mut hash_input),
        );
        if hunks < 0 {
            eprintln!("error: could not parse conflict hunks in '{label}'");
            return (-1, String::new());
        }
        if let Some(output) = output
            && let Err(e) = std::fs::write(output, &out)
        {
            eprintln!(
                "error: could not write '{}': {}",
                output.display(),
                os_error(&e)
            );
            return (-1, String::new());
        }
        let hex = if want_hash {
            let mut hasher = self.format.hasher();
            hasher.update(&hash_input);
            hasher
                .finish()
                .map(|oid| oid.to_hex().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        (hunks, hex)
    }

    // --- the algorithm -------------------------------------------------------------------

    /// Index paths that are conflicted with stages 2 and 3 regular files (`THREE_STAGED`).
    fn find_conflict(&self, repository: &Repository) -> Vec<BString> {
        let mut found = Vec::new();
        let mut walker = ConflictWalk::new(repository);
        while let Some((name, kind)) = walker.next() {
            if kind == ConflictKind::ThreeStaged {
                found.push(name);
            }
        }
        found
    }

    fn do_plain(&mut self, repository: &mut Repository) -> Result<()> {
        let conflicts = self.find_conflict(repository);
        for path in &conflicts {
            let (hunks, hex) = self.handle_file(path, true, None);
            if hunks != 0
                && let Some(Some(old)) = self.rr.get(path).cloned()
            {
                self.remove_variant(&old);
                self.rr.remove(path);
            }
            if hunks < 1 {
                continue;
            }
            let id = Id { hex, variant: -1 };
            let _ = std::fs::create_dir_all(self.dir_path(&id));
            self.rr.insert(path.clone(), Some(id));
        }

        let mut update: Vec<BString> = Vec::new();
        let paths: Vec<BString> = self.rr.keys().cloned().collect();
        for path in paths {
            self.do_one_path(&path, &mut update)?;
        }
        if !update.is_empty() {
            let mut options = rivet_core::worktree::AddOptions::new();
            options.pathspec = BString::default();
            for path in &update {
                repository.add(std::slice::from_ref(path), &options)?;
                if !QUIET.load(Ordering::Relaxed) {
                    eprintln!("Staged '{}' using previous resolution.", path);
                }
            }
        }
        self.write_rr()
    }

    fn do_one_path(&mut self, path: &BString, update: &mut Vec<BString>) -> Result<()> {
        let Some(Some(mut id)) = self.rr.get(path).cloned() else {
            return Ok(());
        };

        // Has the user resolved it already?
        if id.variant >= 0 && self.handle_file(path, false, None).0 == 0 {
            let postimage = self.file_path(&id, "postimage");
            let _ = std::fs::copy(self.root.join(os(path)), &postimage);
            let flags = self.flags(&id) | POSTIMAGE;
            self.set_flags(&id, flags);
            if !QUIET.load(Ordering::Relaxed) {
                eprintln!("Recorded resolution for '{path}'.");
            }
            self.rr.insert(path.clone(), None);
            return Ok(());
        }

        // Does any existing resolution apply cleanly?
        let slots = self.status(&id.hex).len();
        for variant in 0..slots {
            let both = PREIMAGE | POSTIMAGE;
            let probe = Id {
                hex: id.hex.clone(),
                variant: variant as i32,
            };
            if self.flags(&probe) & both != both {
                continue;
            }
            if self.replay(&probe, path) {
                continue;
            }
            if id.variant >= 0 && id.variant != variant as i32 {
                self.remove_variant(&id);
            }
            if self.autoupdate {
                update.push(path.clone());
            } else {
                if !QUIET.load(Ordering::Relaxed) {
                    eprintln!("Resolved '{path}' using previous resolution.");
                }
            }
            self.rr.insert(path.clone(), None);
            return Ok(());
        }

        // None of the existing ones applies: this conflict gets a variant of its own.
        self.assign_variant(&mut id);
        let preimage = self.file_path(&id, "preimage");
        self.handle_file(path, false, Some(&preimage));
        let mut flags = self.flags(&id);
        if flags & POSTIMAGE != 0 {
            let stray = self.file_path(&id, "postimage");
            std::fs::remove_file(&stray).map_err(|e| {
                anyhow::anyhow!(
                    "cannot unlink stray '{}': {}",
                    stray.display(),
                    os_error(&e)
                )
            })?;
            flags &= !POSTIMAGE;
        }
        self.set_flags(&id, flags | PREIMAGE);
        if !QUIET.load(Ordering::Relaxed) {
            eprintln!("Recorded preimage for '{path}'");
        }
        self.rr.insert(path.clone(), Some(id));
        Ok(())
    }

    /// Replay variant `id` on the conflicted `path`. Returns `true` when it failed.
    fn replay(&mut self, id: &Id, path: &BString) -> bool {
        let thisimage = self.file_path(id, "thisimage");
        if self.handle_file(path, false, Some(&thisimage)).0 < 0 {
            return true;
        }
        let (Ok(cur), Ok(base), Ok(other)) = (
            std::fs::read(&thisimage),
            std::fs::read(self.file_path(id, "preimage")),
            std::fs::read(self.file_path(id, "postimage")),
        ) else {
            return true;
        };
        let Ok(merged) = merge_content(
            Some(&base),
            &cur,
            &other,
            ("", ""),
            ConflictStyle::Merge,
            rivet_core::diff::DiffAlgorithm::Myers,
        ) else {
            return true;
        };
        if merged.conflict {
            return true;
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .write(true)
            .open(self.file_path(id, "postimage"))
        {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        if let Err(e) = std::fs::write(self.root.join(os(path)), &merged.data) {
            eprintln!("error: could not open '{path}': {}", os_error(&e));
            return true;
        }
        false
    }
}

fn os_error(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

// --- conflict classification -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConflictKind {
    Resolved,
    Punted,
    ThreeStaged,
}

struct ConflictWalk<'a> {
    entries: &'a [rivet_core::IndexEntry],
    at: usize,
}

impl<'a> ConflictWalk<'a> {
    fn new(repository: &'a Repository) -> Self {
        Self {
            entries: repository.index().entries(),
            at: 0,
        }
    }

    /// git's `check_one_conflict`: classify the entry at the cursor and skip its stages.
    fn next(&mut self) -> Option<(BString, ConflictKind)> {
        let entries = self.entries;
        let mut i = self.at;
        let first = entries.get(i)?;
        let name = BString::from(first.path.as_slice());
        if first.stage() == Stage::Normal {
            self.at = i + 1;
            return Some((name, ConflictKind::Resolved));
        }
        let mut kind = ConflictKind::Punted;
        while i < entries.len() && entries[i].stage() == Stage::Base {
            i += 1;
        }
        if i + 1 < entries.len() {
            let (e2, e3) = (&entries[i], &entries[i + 1]);
            let regular = |e: &rivet_core::IndexEntry| {
                matches!(
                    e.mode,
                    rivet_core::EntryMode::Blob | rivet_core::EntryMode::BlobExecutable
                )
            };
            if e2.stage() == Stage::Ours
                && e3.stage() == Stage::Theirs
                && e3.path == first.path
                && regular(e2)
                && regular(e3)
            {
                kind = ConflictKind::ThreeStaged;
            }
        }
        while i < entries.len() && entries[i].path == first.path {
            i += 1;
        }
        self.at = i;
        Some((name, kind))
    }
}

// --- hunk normalisation ------------------------------------------------------------------

fn is_cmarker(line: &[u8], marker: u8, size: usize) -> bool {
    if line.len() < size || line[..size].iter().any(|&b| b != marker) {
        return false;
    }
    let next = line.get(size).copied();
    match marker {
        b'<' | b'>' => next == Some(b' '),
        b'|' => matches!(next, None | Some(b' ' | b'\n' | b'\r')),
        _ => matches!(next, None | Some(b'\n' | b'\r')),
    }
}

fn put_marker(out: &mut Vec<u8>, marker: u8, size: usize) {
    out.extend(std::iter::repeat_n(marker, size));
    out.push(b'\n');
}

struct Lines<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Option<&'a [u8]> {
        if self.at >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.at..];
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |p| p + 1);
        self.at += end;
        Some(&rest[..end])
    }
}

/// git's `handle_conflict`: the opening marker has been read. `-1` for an unterminated or
/// misordered hunk, `1` after writing the normalised hunk to `out`.
fn handle_conflict(
    lines: &mut Lines<'_>,
    size: usize,
    out: &mut Vec<u8>,
    mut hash: Option<&mut Vec<u8>>,
) -> i32 {
    #[derive(PartialEq)]
    enum Part {
        Side1,
        Original,
        Side2,
    }
    let mut part = Part::Side1;
    let mut one: Vec<u8> = Vec::new();
    let mut two: Vec<u8> = Vec::new();
    while let Some(line) = lines.next_line() {
        if is_cmarker(line, b'<', size) {
            let mut nested = Vec::new();
            if handle_conflict(lines, size, &mut nested, None) < 0 {
                break;
            }
            if part == Part::Side1 {
                one.extend_from_slice(&nested);
            } else {
                two.extend_from_slice(&nested);
            }
        } else if is_cmarker(line, b'|', size) {
            if part != Part::Side1 {
                break;
            }
            part = Part::Original;
        } else if is_cmarker(line, b'=', size) {
            if part != Part::Side1 && part != Part::Original {
                break;
            }
            part = Part::Side2;
        } else if is_cmarker(line, b'>', size) {
            if part != Part::Side2 {
                break;
            }
            if one > two {
                std::mem::swap(&mut one, &mut two);
            }
            put_marker(out, b'<', size);
            out.extend_from_slice(&one);
            put_marker(out, b'=', size);
            out.extend_from_slice(&two);
            put_marker(out, b'>', size);
            if let Some(hash) = hash.as_deref_mut() {
                hash.extend_from_slice(&one);
                hash.push(0);
                hash.extend_from_slice(&two);
                hash.push(0);
            }
            return 1;
        } else if part == Part::Side1 {
            one.extend_from_slice(line);
        } else if part == Part::Side2 {
            two.extend_from_slice(line);
        }
    }
    -1
}

/// git's `handle_path`: normalise every conflict of `data` into `out`; the number of hunks, or
/// `-1` when one is malformed.
fn handle_path(data: &[u8], size: usize, out: &mut Vec<u8>, mut hash: Option<&mut Vec<u8>>) -> i32 {
    let mut lines = Lines { data, at: 0 };
    let mut hunks = 0;
    while let Some(line) = lines.next_line() {
        if is_cmarker(line, b'<', size) {
            let mut one = Vec::new();
            if handle_conflict(&mut lines, size, &mut one, hash.as_deref_mut()) < 0 {
                return -1;
            }
            hunks += 1;
            out.extend_from_slice(&one);
        } else {
            out.extend_from_slice(line);
        }
    }
    hunks
}

// --- the command -------------------------------------------------------------------------

/// `rerere [--[no-]rerere-autoupdate] [clear | forget <pathspec>... | diff | status | remaining | gc]`.
pub fn run_rerere(global: &GlobalArgs, args: &RerereArgs) -> Result<i32> {
    let mut autoupdate = AutoUpdate::Config;
    let mut words: Vec<&str> = Vec::new();
    let mut iter = args.args.iter();
    while let Some(word) = iter.next() {
        match word.as_str() {
            "-h" => {
                print!("{USAGE}");
                return Ok(0);
            }
            "--rerere-autoupdate" => autoupdate = AutoUpdate::Yes,
            "--no-rerere-autoupdate" => autoupdate = AutoUpdate::No,
            "--" => {
                words.extend(iter.by_ref().map(String::as_str));
                break;
            }
            w if w.starts_with("--") => {
                let name = &w[2..];
                let known = ["rerere-autoupdate", "no-rerere-autoupdate"];
                let hits: Vec<&&str> = known.iter().filter(|k| k.starts_with(name)).collect();
                match hits.as_slice() {
                    [one] if **one == "rerere-autoupdate" => autoupdate = AutoUpdate::Yes,
                    [one] => {
                        let _ = one;
                        autoupdate = AutoUpdate::No;
                    }
                    _ => {
                        eprintln!("error: unknown option `{name}'");
                        eprint!("{USAGE}");
                        return Ok(129);
                    }
                }
            }
            w if w.len() > 1 && w.starts_with('-') => {
                eprintln!("error: unknown switch `{}'", &w[1..2]);
                eprint!("{USAGE}");
                return Ok(129);
            }
            w => {
                words.push(w);
                words.extend(iter.by_ref().map(String::as_str));
                break;
            }
        }
    }

    let mut repository = super::open(global)?;
    let Some((&command, rest)) = words.split_first() else {
        run_plain(&mut repository, autoupdate)?;
        return Ok(0);
    };
    match command {
        "forget" => forget(&mut repository, rest),
        "clear" => {
            clear(&repository, autoupdate)?;
            Ok(0)
        }
        "gc" => {
            gc(&repository, autoupdate)?;
            Ok(0)
        }
        "status" => {
            let Some(state) = Rerere::setup(&repository, autoupdate)? else {
                return Ok(0);
            };
            let mut out = std::io::stdout().lock();
            for path in state.rr.keys() {
                out.write_all(path)?;
                out.write_all(b"\n")?;
            }
            Ok(0)
        }
        "remaining" => {
            remaining(&repository, autoupdate)?;
            Ok(0)
        }
        "diff" => diff(&repository, autoupdate),
        _ => {
            eprint!("{USAGE}");
            Ok(129)
        }
    }
}

fn clear(repository: &Repository, autoupdate: AutoUpdate) -> Result<()> {
    let Some(mut state) = Rerere::setup(repository, autoupdate)? else {
        return Ok(());
    };
    let ids: Vec<Id> = state.rr.values().flatten().cloned().collect();
    for id in ids {
        let both = PREIMAGE | POSTIMAGE;
        if state.flags(&id) & both != both {
            state.unlink_item(&id);
            let _ = std::fs::remove_dir(state.dir_path(&id));
        }
    }
    if let Err(e) = std::fs::remove_file(&state.merge_rr)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "warning: unable to unlink '{}': {}",
            state.merge_rr.display(),
            os_error(&e)
        );
    }
    Ok(())
}

/// `git rerere clear` as `am --skip` and `am --abort` run it: forget the unresolved conflicts.
pub fn rerere_clear(repository: &Repository) -> Result<()> {
    clear(repository, AutoUpdate::Config)
}

fn remaining(repository: &Repository, autoupdate: AutoUpdate) -> Result<()> {
    let Some(mut state) = Rerere::setup(repository, autoupdate)? else {
        return Ok(());
    };
    let mut resolved: Vec<BString> = Vec::new();
    let mut walker = ConflictWalk::new(repository);
    while let Some((name, kind)) = walker.next() {
        match kind {
            ConflictKind::Punted => {
                state.rr.entry(name).or_insert(None);
            }
            ConflictKind::Resolved => {
                if state.rr.contains_key(&name) {
                    resolved.push(name);
                }
            }
            ConflictKind::ThreeStaged => {}
        }
    }
    let mut out = std::io::stdout().lock();
    for path in state.rr.keys() {
        if !resolved.contains(path) {
            out.write_all(path)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn diff(repository: &Repository, autoupdate: AutoUpdate) -> Result<i32> {
    let Some(state) = Rerere::setup(repository, autoupdate)? else {
        return Ok(0);
    };
    let mut out = std::io::stdout().lock();
    for (path, id) in &state.rr {
        let Some(id) = id else { continue };
        let old = std::fs::read(state.file_path(id, "preimage")).unwrap_or_default();
        let new = std::fs::read(state.root.join(os(path))).unwrap_or_default();
        out.write_all(&unified_two(path.as_bstr(), &old, &new))?;
    }
    Ok(0)
}

/// What git's `diff_two` prints for two versions of one path: bare `---`/`+++` headers and hunks.
fn unified_two(path: &BStr, old: &[u8], new: &[u8]) -> Vec<u8> {
    use rivet_core::diff::myers::{compact_edits, git_diff, split_lines};
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let edits = git_diff(&old_lines, &new_lines);
    let edits = compact_edits(&old_lines, &new_lines, &edits);
    let hunks = rivet_core::diff::unified::build_hunks(old, new, &edits, 3);
    if hunks.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    out.extend_from_slice(format!("--- a/{path}\n+++ b/{path}\n").as_bytes());
    for hunk in &hunks {
        let _ = hunk.write(&mut out);
    }
    out
}

fn gc(repository: &Repository, autoupdate: AutoUpdate) -> Result<()> {
    let Some(mut state) = Rerere::setup(repository, autoupdate)? else {
        return Ok(());
    };
    let days = |key: &str, default: i64| -> i64 {
        match repository.config().get(BStr::new(key)) {
            Ok(Some(value)) => match value.to_str().unwrap_or("") {
                "now" => 0,
                "never" | "false" => i64::MAX / 86400 / 2,
                other => other.parse::<i64>().unwrap_or(default),
            },
            _ => default,
        }
    };
    let resolved_days = days("gc.rerereresolved", 60);
    let unresolved_days = days("gc.rerereunresolved", 15);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff_resolve = now.saturating_sub(resolved_days.saturating_mul(86400));
    let cutoff_noresolve = now.saturating_sub(unresolved_days.saturating_mul(86400));
    let mtime = |path: PathBuf| -> i64 {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64)
    };

    let Ok(entries) = std::fs::read_dir(&state.cache) else {
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.len() == state.format.hex_len() && n.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    names.sort();
    for hex in names {
        let slots = state.status(&hex).len();
        let mut now_empty = true;
        for variant in 0..slots {
            let id = Id {
                hex: hex.clone(),
                variant: variant as i32,
            };
            let used = mtime(state.file_path(&id, "postimage"));
            let (then, cutoff) = if used != 0 {
                (used, cutoff_resolve)
            } else {
                let created = mtime(state.file_path(&id, "preimage"));
                if created == 0 {
                    if state.flags(&id) != 0 {
                        now_empty = false;
                    }
                    continue;
                }
                (created, cutoff_noresolve)
            };
            if then < cutoff {
                state.unlink_item(&id);
            }
            if state.flags(&id) != 0 {
                now_empty = false;
            }
        }
        if now_empty {
            let _ = std::fs::remove_dir(state.cache.join(&hex));
        }
    }
    Ok(())
}

// --- forget ------------------------------------------------------------------------------

/// The three index stages of `path` merged the way git's `handle_cache` does it; the merged text
/// carries conflict markers.
fn merge_stages(repository: &Repository, path: &BStr) -> Option<Vec<u8>> {
    let index = repository.index();
    let blob = |stage: Stage| -> Option<Vec<u8>> {
        let entry = index.entry_at(path, stage).ok().flatten()?;
        repository
            .odb()
            .read(&entry.oid)
            .ok()
            .map(|o| o.data().to_vec())
    };
    let base = blob(Stage::Base);
    let ours = blob(Stage::Ours)?;
    let theirs = blob(Stage::Theirs)?;
    merge_content(
        base.as_deref(),
        &ours,
        &theirs,
        ("ours", "theirs"),
        ConflictStyle::Merge,
        rivet_core::diff::DiffAlgorithm::Myers,
    )
    .ok()
    .map(|m| m.data.to_vec())
}

fn forget(repository: &mut Repository, specs: &[&str]) -> Result<i32> {
    if specs.is_empty() {
        eprintln!("warning: 'git rerere forget' without paths is deprecated");
    }
    let Some(mut state) = Rerere::setup(repository, AutoUpdate::No)? else {
        return Ok(0);
    };
    let mut patterns: Vec<BString> = Vec::new();
    for spec in specs {
        patterns.push(super::worktree_path(repository, Path::new(spec), "rerere")?);
    }
    // Paths already staged are put back into their conflicted state, in memory only, from the
    // resolve-undo extension.
    let format = repository.format();
    let resolved: Vec<BString> = repository
        .index()
        .resolve_undo(format)
        .into_iter()
        .map(|r| r.path)
        .filter(|p| {
            patterns.is_empty()
                || patterns
                    .iter()
                    .any(|q| rivet_core::ops::pathspec_matches(p.as_bstr(), q.as_bstr()))
        })
        .collect();
    for path in &resolved {
        let staged = repository
            .index()
            .entry(path.as_bstr())
            .is_some_and(|e| e.stage() == Stage::Normal);
        if staged {
            repository.index_mut().unmerge_path(path.as_bstr(), format);
        }
    }
    let conflicts = state.find_conflict(repository);
    for path in conflicts {
        if !patterns.is_empty()
            && !patterns
                .iter()
                .any(|p| rivet_core::ops::pathspec_matches(path.as_bstr(), p.as_bstr()))
        {
            continue;
        }
        forget_one(repository, &mut state, &path);
    }
    state.write_rr()?;
    Ok(0)
}

fn forget_one(repository: &Repository, state: &mut Rerere, path: &BString) {
    let parse_error = || eprintln!("error: could not parse conflict hunks in '{path}'");
    let Some(merged) = merge_stages(repository, path.as_bstr()) else {
        return parse_error();
    };
    let (hunks, hex) = state.handle_bytes(&merged, path.as_bstr(), true, None);
    if hunks < 1 {
        return;
    }
    let slots = state.status(&hex).len();
    let mut found: Option<Id> = None;
    for variant in 0..slots {
        let id = Id {
            hex: hex.clone(),
            variant: variant as i32,
        };
        if state.flags(&id) & PREIMAGE == 0 {
            continue;
        }
        let thisimage = state.file_path(&id, "thisimage");
        let (hunks, _) = state.handle_bytes(&merged, path.as_bstr(), false, Some(&thisimage));
        if hunks < 1 {
            return;
        }
        let (Ok(cur), Ok(base), Ok(other)) = (
            std::fs::read(&thisimage),
            std::fs::read(state.file_path(&id, "preimage")),
            std::fs::read(state.file_path(&id, "postimage")),
        ) else {
            continue;
        };
        let clean = merge_content(
            Some(&base),
            &cur,
            &other,
            ("", ""),
            ConflictStyle::Merge,
            rivet_core::diff::DiffAlgorithm::Myers,
        )
        .is_ok_and(|m| !m.conflict);
        if clean {
            found = Some(id);
            break;
        }
    }
    let Some(id) = found else {
        eprintln!("error: no remembered resolution for '{path}'");
        return;
    };
    let postimage = state.file_path(&id, "postimage");
    if let Err(e) = std::fs::remove_file(&postimage) {
        if e.kind() == std::io::ErrorKind::NotFound {
            eprintln!("error: no remembered resolution for '{path}'");
        } else {
            eprintln!(
                "error: cannot unlink '{}': {}",
                postimage.display(),
                os_error(&e)
            );
        }
        return;
    }
    let preimage = state.file_path(&id, "preimage");
    state.handle_bytes(&merged, path.as_bstr(), false, Some(&preimage));
    eprintln!("Updated preimage for '{path}'");
    let flags = state.flags(&id) & !POSTIMAGE;
    state.set_flags(&id, flags);
    state.rr.insert(path.clone(), Some(id));
    eprintln!("Forgot resolution for '{path}'");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normalise `text` and return the hunk count, the normalised text and the conflict id.
    fn scan(text: &[u8]) -> (i32, Vec<u8>, String) {
        let mut out = Vec::new();
        let mut hash_input = Vec::new();
        let hunks = handle_path(text, MARKER_SIZE, &mut out, Some(&mut hash_input));
        let mut hasher = ObjectFormat::Sha1.hasher();
        hasher.update(&hash_input);
        (hunks, out, hasher.finish().unwrap().to_hex().to_string())
    }

    // The id git itself assigned to this conflict (`rr-cache/27d08dca…`) when it was built with
    // /usr/bin/git: `MAIN` against `SIDE`, whatever the labels or the order of the sides.
    const GIT_ID: &str = "27d08dca9e6e45868a32cd28625ad61784a53d0e";

    #[test]
    fn conflict_id_is_gits_and_ignores_labels_order_and_base() {
        let plain = b"a\nb\n<<<<<<< HEAD\nMAIN\n=======\nSIDE\n>>>>>>> side\nd\ne\n";
        let swapped = b"a\nb\n<<<<<<< side\nSIDE\n=======\nMAIN\n>>>>>>> HEAD\nd\ne\n";
        let diff3 =
            b"a\nb\n<<<<<<< HEAD\nMAIN\n||||||| base\nc\n=======\nSIDE\n>>>>>>> side\nd\ne\n";
        for text in [&plain[..], &swapped[..], &diff3[..]] {
            let (hunks, out, id) = scan(text);
            assert_eq!(hunks, 1);
            assert_eq!(id, GIT_ID);
            assert_eq!(
                out,
                b"a\nb\n<<<<<<<\nMAIN\n=======\nSIDE\n>>>>>>>\nd\ne\n".to_vec()
            );
        }
    }

    #[test]
    fn unterminated_or_misordered_hunks_are_parse_errors() {
        assert_eq!(scan(b"<<<<<<< a\nx\n=======\ny\n").0, -1);
        assert_eq!(scan(b"<<<<<<< a\nx\n>>>>>>> b\n").0, -1);
        assert_eq!(scan(b"no conflict here\n").0, 0);
    }

    #[test]
    fn a_marker_needs_its_full_width_and_a_space_after_angle_brackets() {
        assert_eq!(scan(b"<<<<<< a\nx\n=======\ny\n>>>>>>> b\n").0, 0);
        assert_eq!(scan(b"<<<<<<<a\nx\n=======\ny\n>>>>>>> b\n").0, 0);
        assert_eq!(scan(b"<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n").0, 1);
    }
}
