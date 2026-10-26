//! `blame`: attribute each line of a file to the commit that last changed it.
//!
//! This follows git's `blame.c`: the final file is a list of *entries* (runs of lines that share
//! a suspect); a work queue ordered by commit date hands each suspect's entries to its parents
//! (`pass_blame`): the lines a parent also has, by a line diff, move to the parent; the rest stay
//! blamed on the commit. With `-M` the entries that stayed are looked for elsewhere in the same
//! parent file, with `-C` in the other files the commit changed (`-C -C`: every file of the
//! parent when the commit created the file, `-C -C -C`: always), picking in each case the best
//! scoring run of lines that the diff aligns (`find_copy_in_blob`), with git's thresholds
//! (20 and 40 alphanumeric characters by default).
//!
//! Not implemented: `--since`, `-S`, `--contents`, `--reverse`,
//! function-name `-L` ranges, `-M`/`-C` while the work tree differs from the commit.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::rc::Rc;

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::diff::myers::{Edit, compact_edits, git_diff, split_lines};
use rivet_core::object::EntryMode;
use rivet_core::{ObjectId, ObjectKind, Repository};

use crate::cli::GlobalArgs;

/// Arguments of `blame`.
#[derive(Debug, clap::Args)]
pub struct BlameArgs {
    /// `[<rev>] [--] <file>`.
    #[arg(value_name = "ARG", required = true)]
    pub args: Vec<String>,

    /// Only the given line range: `N,M`, `N,+K`, `N,-K`, `N` or `,M`; may be repeated.
    #[arg(short = 'L', value_name = "RANGE")]
    pub ranges: Vec<String>,

    /// Show the author's email instead of the name.
    #[arg(short = 'e', long = "show-email")]
    pub email: bool,

    /// Suppress the author name and the timestamp.
    #[arg(short = 's')]
    pub suppress: bool,

    /// Show the full object name.
    #[arg(short = 'l')]
    pub long: bool,

    /// Show the file name of each line's origin.
    #[arg(short = 'f', long = "show-name")]
    pub show_name: bool,

    /// Machine-readable output: one header per group of lines, commit details once per commit.
    #[arg(short = 'p', long = "porcelain")]
    pub porcelain: bool,

    /// Like `--porcelain`, with the commit details repeated for every line.
    #[arg(long = "line-porcelain")]
    pub line_porcelain: bool,

    /// Report each group of lines as soon as its commit is found, without the lines themselves.
    #[arg(long = "incremental")]
    pub incremental: bool,

    /// Ignore whitespace when comparing a commit with its parent.
    #[arg(short = 'w')]
    pub ignore_whitespace: bool,

    /// `-M`: detect lines moved within a file (`-M<score>` sets the threshold).
    #[arg(long = "detect-moves", hide = true)]
    pub detect_moves: bool,

    /// The `-M<score>` threshold (alphanumeric characters).
    #[arg(long = "move-score", hide = true, value_name = "N", action = clap::ArgAction::Append)]
    pub move_score: Vec<usize>,

    /// `-C`: detect lines copied from other files (repeat for a wider search).
    #[arg(long = "detect-copies", hide = true, action = clap::ArgAction::Count)]
    pub detect_copies: u8,

    /// The `-C<score>` threshold (alphanumeric characters).
    #[arg(long = "copy-score", hide = true, value_name = "N", action = clap::ArgAction::Append)]
    pub copy_score: Vec<usize>,
}

fn parse_range(text: &str, total: usize, file: &str) -> Result<(usize, usize)> {
    let fail = || anyhow::anyhow!("-L argument not 'start,end': '{text}'");
    let number = |s: &str| s.trim().parse::<usize>().map_err(|_| fail());
    let (start, end) = match text.split_once(',') {
        None => (number(text)?, total),
        Some((a, b)) => {
            let a = if a.is_empty() { 1 } else { number(a)? };
            let b = if b.is_empty() {
                total
            } else if let Some(rest) = b.strip_prefix('+') {
                a + number(rest)?.saturating_sub(1)
            } else if let Some(rest) = b.strip_prefix('-') {
                let count = number(rest)?;
                return Ok((a.saturating_sub(count.saturating_sub(1)).max(1), a));
            } else {
                number(b)?
            };
            (a, b)
        }
    };
    let (start, end) = if start > end {
        (end, start)
    } else {
        (start, end)
    };
    if start == 0 || start > total {
        anyhow::bail!("file {file} has only {total} lines");
    }
    Ok((start, end.min(total)))
}

/// Why a `-L` argument was rejected: git dies with a message, or prints its usage.
enum RangeError {
    Fatal(String),
    Usage,
}

/// Index just past the `/` that closes the regular expression opening at `text[0] == '/'`;
/// a backslash escapes the next byte, as in git's `parse_loc`.
fn regex_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'/' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The first line at or after `from` (1-based) matching the regular expression `pattern`.
fn search_line(pattern: &str, lines: &[&[u8]], from: usize) -> Result<usize, RangeError> {
    let fatal = |why: &str| {
        RangeError::Fatal(format!(
            "-L parameter '{pattern}' starting at line {from}: {why}"
        ))
    };
    let regex = super::regex::Regex::new(pattern.as_bytes(), false, false)
        .map_err(|error| fatal(&error.to_string()))?;
    lines
        .iter()
        .skip(from - 1)
        .position(|line| regex.is_match(line, false))
        .map(|found| from + found)
        .ok_or_else(|| fatal("No match"))
}

/// `-L` ranges that involve a regular expression (`/re/`, `^/re/`, `<start>,/re/`, `/re/,<end>`);
/// `anchor` is the line after the previous range, where an unanchored start search begins.
fn parse_regex_range(
    text: &str,
    lines: &[&[u8]],
    anchor: usize,
    file: &str,
) -> Result<(usize, usize), RangeError> {
    let total = lines.len();
    let (start, rest) = if text.starts_with("^/") || text.starts_with('/') {
        let (body, from) = match text.strip_prefix('^') {
            Some(body) => (body, 1),
            None => (text, anchor.clamp(1, total + 1)),
        };
        let end = regex_end(body).ok_or(RangeError::Usage)?;
        (search_line(&body[1..end - 1], lines, from)?, &body[end..])
    } else {
        let (first, _) = text.split_once(',').ok_or(RangeError::Usage)?;
        // An empty start is line 0 here: the end search begins at line 1 and the range at line 1.
        let start = match first.trim() {
            "" => 0,
            number => number.parse::<usize>().map_err(|_| RangeError::Usage)?,
        };
        if start == 0 && !first.trim().is_empty() {
            return Err(RangeError::Fatal("-L invalid line number: 0".to_string()));
        }
        (start, &text[first.len()..])
    };
    let spec = match rest {
        "" => "",
        _ => rest.strip_prefix(',').ok_or(RangeError::Usage)?,
    };
    let too_short = || RangeError::Fatal(format!("file {file} has only {total} lines"));
    let number = |s: &str| s.parse::<usize>().map_err(|_| RangeError::Usage);
    let end = if spec.is_empty() {
        total
    } else if spec.starts_with('/') {
        if regex_end(spec) != Some(spec.len()) {
            return Err(RangeError::Usage);
        }
        if start > total {
            return Err(too_short());
        }
        search_line(&spec[1..spec.len() - 1], lines, start + 1)?
    } else if let Some(count) = spec.strip_prefix('+') {
        match number(count)? {
            0 => return Err(RangeError::Fatal("-L invalid empty range".to_string())),
            count => start + count - 1,
        }
    } else if let Some(count) = spec.strip_prefix('-') {
        let count = number(count)?;
        if start > total {
            return Err(too_short());
        }
        return Ok((start.saturating_sub(count.saturating_sub(1)).max(1), start));
    } else {
        number(spec)?
    };
    let (start, end) = if start > end {
        (end, start)
    } else {
        (start, end)
    };
    let start = start.max(1);
    if start > total {
        return Err(too_short());
    }
    Ok((start, end.min(total)))
}

type TreeMap = BTreeMap<BString, (EntryMode, ObjectId)>;

/// A run of final-file lines that currently share a suspect.
#[derive(Clone, Debug)]
struct Entry {
    /// First line in the final file.
    lno: usize,
    /// Number of lines.
    num: usize,
    /// The origin (commit, path) the lines are blamed on.
    suspect: usize,
    /// First line in the suspect's version of the file.
    s_lno: usize,
}

/// A version of a file in one commit (or in the work tree).
struct Origin {
    commit: Option<ObjectId>,
    path: Vec<u8>,
    blob: ObjectId,
    data: Rc<Vec<u8>>,
    /// The parent version this one was first compared with (`previous` in porcelain output).
    previous: Option<(ObjectId, Vec<u8>)>,
    suspects: Vec<Entry>,
    boundary: bool,
}

struct Options {
    ignore_ws: bool,
    mov: bool,
    copy: bool,
    harder: bool,
    hardest: bool,
    move_score: usize,
    copy_score: usize,
}

/// The line diff blame runs between two versions, compacted like git's. With `ignore_ws` every
/// whitespace byte (`xdl_isspace`) is dropped from each line first, which is what `-w` compares.
fn line_edits(old: &[u8], new: &[u8], ignore_ws: bool) -> Vec<Edit> {
    fn squeeze(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for line in data.split_inclusive(|&b| b == b'\n') {
            out.extend(
                line.iter()
                    .filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')),
            );
            out.push(b'\n');
        }
        out
    }
    let (old, new) = if ignore_ws {
        (squeeze(old), squeeze(new))
    } else {
        (old.to_vec(), new.to_vec())
    };
    let old_lines = split_lines(&old);
    let new_lines = split_lines(&new);
    let edits = git_diff(&old_lines, &new_lines);
    compact_edits(&old_lines, &new_lines, &edits)
}

struct Blamer<'r> {
    repo: &'r Repository,
    final_data: Vec<u8>,
    line_starts: Vec<usize>,
    origins: Vec<Origin>,
    by_key: HashMap<(Option<ObjectId>, Vec<u8>), usize>,
    commit_origins: HashMap<Option<ObjectId>, Vec<usize>>,
    queue: BinaryHeap<(i64, Reverse<u64>, ObjectId)>,
    in_queue: HashSet<ObjectId>,
    counter: u64,
    dates: HashMap<ObjectId, i64>,
    trees: HashMap<ObjectId, Rc<TreeMap>>,
    tip: ObjectId,
    tip_tree: ObjectId,
    final_entries: Vec<Entry>,
    /// The entries in the order their suspects were settled (before `coalesce`).
    guilty: Vec<Entry>,
    options: Options,
}

type Split = [Option<Entry>; 3];

/// `split_overlap`: the part of `e` before the matching run, the run itself (handed to `parent`)
/// and the part after it. `tlno..same` is the run in the entry's own file, `plno` its start in
/// the parent.
fn split_overlap(e: &Entry, tlno: usize, plno: usize, same: usize, parent: usize) -> Split {
    let mut split: Split = [None, None, None];
    let (middle_lno, middle_s);
    if e.s_lno < tlno {
        split[0] = Some(Entry {
            lno: e.lno,
            num: tlno - e.s_lno,
            suspect: e.suspect,
            s_lno: e.s_lno,
        });
        middle_lno = e.lno + tlno - e.s_lno;
        middle_s = plno;
    } else {
        middle_lno = e.lno;
        middle_s = plno + (e.s_lno - tlno);
    }
    let chunk_end = if same < e.s_lno + e.num {
        let lno = e.lno + (same - e.s_lno);
        split[2] = Some(Entry {
            lno,
            num: e.s_lno + e.num - same,
            suspect: e.suspect,
            s_lno: same,
        });
        lno
    } else {
        e.lno + e.num
    };
    if chunk_end <= middle_lno {
        return [None, None, None];
    }
    split[1] = Some(Entry {
        lno: middle_lno,
        num: chunk_end - middle_lno,
        suspect: parent,
        s_lno: middle_s,
    });
    split
}

impl<'r> Blamer<'r> {
    fn score(&self, e: &Entry) -> usize {
        let text = &self.final_data[self.line_starts[e.lno]..self.line_starts[e.lno + e.num]];
        1 + text.iter().filter(|b| b.is_ascii_alphanumeric()).count()
    }

    fn tree_map(&mut self, tree: ObjectId) -> Result<Rc<TreeMap>> {
        if let Some(map) = self.trees.get(&tree) {
            return Ok(map.clone());
        }
        let map = Rc::new(rivet_core::ops::flatten_tree(self.repo.odb(), tree)?);
        self.trees.insert(tree, map.clone());
        Ok(map)
    }

    fn commit_date(&mut self, commit: ObjectId) -> Result<i64> {
        if let Some(date) = self.dates.get(&commit) {
            return Ok(*date);
        }
        let object = self.repo.odb().read_typed(&commit, ObjectKind::Commit)?;
        let parsed = rivet_core::Commit::parse(self.repo.format(), object.data())?;
        let date = parsed.committer.when;
        self.dates.insert(commit, date);
        Ok(date)
    }

    /// `get_origin`: the origin of `path` in `commit`, created on first use.
    fn get_origin(
        &mut self,
        commit: Option<ObjectId>,
        path: &[u8],
        blob: ObjectId,
    ) -> Result<usize> {
        let key = (commit, path.to_vec());
        if let Some(index) = self.by_key.get(&key) {
            return Ok(*index);
        }
        let data = if blob == ObjectId::zero(self.repo.format()) {
            Rc::new(Vec::new())
        } else {
            Rc::new(
                self.repo
                    .odb()
                    .read_typed(&blob, ObjectKind::Blob)?
                    .into_data(),
            )
        };
        let index = self.origins.len();
        self.origins.push(Origin {
            commit,
            path: path.to_vec(),
            blob,
            data,
            previous: None,
            suspects: Vec::new(),
            boundary: false,
        });
        self.by_key.insert(key, index);
        self.commit_origins.entry(commit).or_default().push(index);
        Ok(index)
    }

    /// `find_origin`: where the file of `origin` was in `parent`, following a rename.
    fn find_origin(&mut self, parent: ObjectId, origin: usize) -> Result<Option<usize>> {
        let path = self.origins[origin].path.clone();
        let commit = self.origins[origin].commit;
        let parent_tree = self.repo.commit_tree(parent)?;
        let map = self.tree_map(parent_tree)?;
        if let Some((_, blob)) = map.get(BStr::new(&path)) {
            return Ok(Some(self.get_origin(Some(parent), &path, *blob)?));
        }
        let child_tree = match commit {
            Some(oid) => self.repo.commit_tree(oid)?,
            None => self.tip_tree,
        };
        let options = rivet_core::DiffOptions::new();
        let mut diff = rivet_core::tree_diff(
            self.repo.format(),
            parent_tree,
            child_tree,
            self.repo.odb(),
            &options,
        )?;
        super::detect_renames(self.repo, &mut diff, &options)?;
        for file in &diff.files {
            if file.is_rename()
                && file.new_path.as_deref().map(|p| &p[..]) == Some(&path[..])
                && let Some(old) = &file.old_path
                && let Some((_, blob)) = map.get(BStr::new(&old[..]))
            {
                let old = old.to_vec();
                return Ok(Some(self.get_origin(Some(parent), &old, *blob)?));
            }
        }
        Ok(None)
    }

    /// `queue_blames`: hand `entries` (sorted) to `target`, and queue its commit if needed.
    fn queue_blames(&mut self, target: usize, entries: Vec<Entry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let merged = &mut self.origins[target].suspects;
        merged.extend(entries);
        merged.sort_by_key(|e| e.lno);
        if let Some(commit) = self.origins[target].commit
            && self.in_queue.insert(commit)
        {
            let date = self.commit_date(commit)?;
            self.queue.push((date, Reverse(self.counter), commit));
            self.counter += 1;
        }
        Ok(())
    }

    /// `pass_blame_to_parent`: lines of `target`'s entries that the line diff leaves unchanged
    /// move to `parent`; the others stay.
    fn pass_blame_to_parent(&mut self, target: usize, parent: usize) -> Result<()> {
        let parent_data = self.origins[parent].data.clone();
        let target_data = self.origins[target].data.clone();
        let edits = line_edits(&parent_data, &target_data, self.options.ignore_ws);
        // Unchanged runs: (start in the target, start in the parent, length).
        let runs: Vec<(usize, usize, usize)> = edits
            .iter()
            .filter(|e| !e.is_change() && e.new_len > 0)
            .map(|e| (e.new_index, e.old_index, e.new_len))
            .collect();
        let mut stay: Vec<Entry> = Vec::new();
        let mut passed: Vec<Entry> = Vec::new();
        for e in std::mem::take(&mut self.origins[target].suspects) {
            let end = e.s_lno + e.num;
            let mut cursor = e.s_lno;
            let first = runs.partition_point(|(t, _, len)| t + len <= e.s_lno);
            for (t, p, len) in &runs[first..] {
                if *t >= end {
                    break;
                }
                let a = (*t).max(e.s_lno);
                let b = (t + len).min(end);
                if cursor < a {
                    stay.push(Entry {
                        lno: e.lno + (cursor - e.s_lno),
                        num: a - cursor,
                        suspect: e.suspect,
                        s_lno: cursor,
                    });
                }
                passed.push(Entry {
                    lno: e.lno + (a - e.s_lno),
                    num: b - a,
                    suspect: parent,
                    s_lno: p + (a - t),
                });
                cursor = b;
            }
            if cursor < end {
                stay.push(Entry {
                    lno: e.lno + (cursor - e.s_lno),
                    num: end - cursor,
                    suspect: e.suspect,
                    s_lno: cursor,
                });
            }
        }
        stay.sort_by_key(|e| e.lno);
        self.origins[target].suspects = stay;
        self.queue_blames(parent, passed)
    }

    /// `find_copy_in_blob`: the best scoring run of `ent`'s lines that the parent file has, split
    /// out of the entry.
    fn find_copy_in_blob(&self, ent: &Entry, parent_data: &[u8], parent: usize) -> Split {
        let text = &self.final_data[self.line_starts[ent.lno]..self.line_starts[ent.lno + ent.num]];
        let edits = line_edits(parent_data, text, self.options.ignore_ws);
        let mut best: Split = [None, None, None];
        for edit in edits.iter().filter(|e| !e.is_change() && e.new_len > 0) {
            let tlno = ent.s_lno + edit.new_index;
            let potential = split_overlap(ent, tlno, edit.old_index, tlno + edit.new_len, parent);
            self.copy_split_if_better(&mut best, potential);
        }
        best
    }

    fn copy_split_if_better(&self, best: &mut Split, potential: Split) {
        let Some(new_middle) = &potential[1] else {
            return;
        };
        if let Some(old_middle) = &best[1]
            && self.score(new_middle) < self.score(old_middle)
        {
            return;
        }
        *best = potential;
    }

    /// `find_move_in_parent`: look for what remains of `target` elsewhere in the parent's copy of
    /// the same file.
    fn find_move_in_parent(
        &mut self,
        target: usize,
        parent: usize,
        blames: &mut Vec<Entry>,
    ) -> Result<()> {
        let parent_data = self.origins[parent].data.clone();
        let mut work = std::mem::take(&mut self.origins[target].suspects);
        work.reverse();
        let mut leftover = Vec::new();
        while let Some(e) = work.pop() {
            let split = self.find_copy_in_blob(&e, &parent_data, parent);
            match &split[1] {
                Some(middle) if self.options.move_score < self.score(middle) => {
                    blames.push(middle.clone());
                    for piece in [&split[0], &split[2]].into_iter().flatten() {
                        work.push(piece.clone());
                    }
                }
                _ => leftover.push(e),
            }
        }
        leftover.sort_by_key(|e| e.lno);
        self.origins[target].suspects = leftover;
        Ok(())
    }

    /// `filter_small`: entries scoring at most `min` are set aside, they stay with the commit.
    fn filter_small(&self, small: &mut Vec<Entry>, source: &mut Vec<Entry>, min: usize) {
        let mut keep = Vec::new();
        for e in std::mem::take(source) {
            if self.score(&e) <= min {
                small.push(e);
            } else {
                keep.push(e);
            }
        }
        *source = keep;
    }

    /// `find_copy_in_parent`: look for what remains of `target` in the other files of the parent.
    fn find_copy_in_parent(
        &mut self,
        target: usize,
        parent: ObjectId,
        porigin: Option<usize>,
        toosmall: &mut Vec<Entry>,
        blames: &mut Vec<Entry>,
    ) -> Result<()> {
        let parent_tree = self.repo.commit_tree(parent)?;
        let parent_map = self.tree_map(parent_tree)?;
        let child_tree = match self.origins[target].commit {
            Some(oid) => self.repo.commit_tree(oid)?,
            None => self.tip_tree,
        };
        let child_map = self.tree_map(child_tree)?;
        let target_path = self.origins[target].path.clone();
        let same_path = porigin.is_some_and(|p| self.origins[p].path == target_path);
        let everything = self.options.hardest || (self.options.harder && !same_path);
        // The files git's diff queue offers as sources: with "find copies harder" every file
        // of the parent, otherwise those the commit changed or removed.
        let mut candidates: Vec<(Vec<u8>, ObjectId)> = Vec::new();
        for (path, (mode, blob)) in parent_map.iter() {
            if *mode == EntryMode::Commit {
                continue;
            }
            if !everything && child_map.get(BStr::new(path.as_slice())) == Some(&(*mode, *blob)) {
                continue;
            }
            if porigin.is_some_and(|p| self.origins[p].path == path.as_slice()) {
                continue;
            }
            candidates.push((path.to_vec(), *blob));
        }
        let mut unblamed = std::mem::take(&mut self.origins[target].suspects);
        let mut leftover: Vec<Entry> = Vec::new();
        loop {
            let mut bests: Vec<Split> = vec![[None, None, None]; unblamed.len()];
            for (path, blob) in &candidates {
                let norigin = self.get_origin(Some(parent), path, *blob)?;
                let data = self.origins[norigin].data.clone();
                for (j, ent) in unblamed.iter().enumerate() {
                    let potential = self.find_copy_in_blob(ent, &data, norigin);
                    self.copy_split_if_better(&mut bests[j], potential);
                }
            }
            let mut next: Vec<Entry> = Vec::new();
            for (ent, split) in unblamed.into_iter().zip(bests) {
                match &split[1] {
                    Some(middle) if self.options.copy_score < self.score(middle) => {
                        blames.push(middle.clone());
                        for piece in [&split[0], &split[2]].into_iter().flatten() {
                            next.push(piece.clone());
                        }
                    }
                    _ => leftover.push(ent),
                }
            }
            self.filter_small(toosmall, &mut next, self.options.copy_score);
            if next.is_empty() {
                break;
            }
            unblamed = next;
        }
        leftover.sort_by_key(|e| e.lno);
        self.origins[target].suspects = leftover;
        Ok(())
    }

    fn distribute_blame(&mut self, blames: Vec<Entry>) -> Result<()> {
        let mut by_suspect: BTreeMap<usize, Vec<Entry>> = BTreeMap::new();
        for e in blames {
            by_suspect.entry(e.suspect).or_default().push(e);
        }
        for (suspect, mut entries) in by_suspect {
            entries.sort_by_key(|e| e.lno);
            self.queue_blames(suspect, entries)?;
        }
        Ok(())
    }

    /// `pass_blame`: settle what can be settled for `origin`'s entries by looking at its parents.
    fn pass_blame(&mut self, origin: usize) -> Result<()> {
        let commit = self.origins[origin].commit;
        let parents = match commit {
            Some(oid) => self.repo.commit_parents(oid)?,
            None => vec![self.tip],
        };
        if parents.is_empty() {
            self.origins[origin].boundary = true;
            return Ok(());
        }
        let mut sg_origin: Vec<Option<usize>> = vec![None; parents.len()];
        for (i, parent) in parents.iter().enumerate() {
            let Some(porigin) = self.find_origin(*parent, origin)? else {
                continue;
            };
            if self.origins[porigin].blob == self.origins[origin].blob {
                // Identical file: every line passes.
                let mut all = std::mem::take(&mut self.origins[origin].suspects);
                for e in &mut all {
                    e.suspect = porigin;
                }
                if self.origins[origin].previous.is_none() {
                    self.origins[origin].previous =
                        Some((*parent, self.origins[porigin].path.clone()));
                }
                return self.queue_blames(porigin, all);
            }
            let same = sg_origin[..i]
                .iter()
                .flatten()
                .any(|j| self.origins[*j].blob == self.origins[porigin].blob);
            if !same {
                sg_origin[i] = Some(porigin);
            }
        }

        let mut toosmall: Vec<Entry> = Vec::new();
        let mut blames: Vec<Entry> = Vec::new();
        let mut finished = false;
        for (i, parent) in parents.iter().enumerate() {
            let Some(porigin) = sg_origin[i] else {
                continue;
            };
            if self.origins[origin].previous.is_none() {
                self.origins[origin].previous = Some((*parent, self.origins[porigin].path.clone()));
            }
            self.pass_blame_to_parent(origin, porigin)?;
            if self.origins[origin].suspects.is_empty() {
                finished = true;
                break;
            }
        }

        if !finished && self.options.mov {
            let mut suspects = std::mem::take(&mut self.origins[origin].suspects);
            self.filter_small(&mut toosmall, &mut suspects, self.options.move_score);
            self.origins[origin].suspects = suspects;
            if !self.origins[origin].suspects.is_empty() {
                for porigin in sg_origin.iter().flatten().copied().collect::<Vec<_>>() {
                    self.find_move_in_parent(origin, porigin, &mut blames)?;
                    if self.origins[origin].suspects.is_empty() {
                        break;
                    }
                }
            }
        }

        if !finished && self.options.copy {
            let copy = self.options.copy_score;
            let mov = self.options.move_score;
            if copy > mov {
                let mut suspects = std::mem::take(&mut self.origins[origin].suspects);
                self.filter_small(&mut toosmall, &mut suspects, copy);
                self.origins[origin].suspects = suspects;
            } else if copy < mov {
                let mut suspects = std::mem::take(&mut self.origins[origin].suspects);
                suspects.append(&mut toosmall);
                self.filter_small(&mut toosmall, &mut suspects, copy);
                self.origins[origin].suspects = suspects;
            }
            if !self.origins[origin].suspects.is_empty() {
                for (i, parent) in parents.iter().enumerate() {
                    self.find_copy_in_parent(
                        origin,
                        *parent,
                        sg_origin[i],
                        &mut toosmall,
                        &mut blames,
                    )?;
                    if self.origins[origin].suspects.is_empty() {
                        break;
                    }
                }
            }
        }

        self.distribute_blame(blames)?;
        // What was set aside stays with this commit.
        let mut suspects = std::mem::take(&mut self.origins[origin].suspects);
        suspects.append(&mut toosmall);
        suspects.sort_by_key(|e| e.lno);
        self.origins[origin].suspects = suspects;
        Ok(())
    }

    fn assign_blame(&mut self, first: usize) -> Result<()> {
        self.pass_blame(first)?;
        let entries = std::mem::take(&mut self.origins[first].suspects);
        self.guilty.extend(entries.iter().cloned());
        self.final_entries.extend(entries);
        while let Some((_, _, commit)) = self.queue.pop() {
            self.in_queue.remove(&commit);
            let origins = self
                .commit_origins
                .get(&Some(commit))
                .cloned()
                .unwrap_or_default();
            for origin in origins.into_iter().rev() {
                if self.origins[origin].suspects.is_empty() {
                    continue;
                }
                self.pass_blame(origin)?;
                let entries = std::mem::take(&mut self.origins[origin].suspects);
                self.guilty.extend(entries.iter().cloned());
                self.final_entries.extend(entries);
            }
        }
        self.final_entries.sort_by_key(|e| e.lno);
        // `coalesce`: neighbours of one origin that continue each other form one group.
        let mut grouped: Vec<Entry> = Vec::new();
        for e in std::mem::take(&mut self.final_entries) {
            match grouped.last_mut() {
                Some(last) if last.suspect == e.suspect && last.s_lno + last.num == e.s_lno => {
                    last.num += e.num;
                }
                _ => grouped.push(e),
            }
        }
        self.final_entries = grouped;
        Ok(())
    }
}

/// Handler for `rivet blame`.
///
/// # Exit codes
///
/// `0`; `128` for an unknown revision or path or a bad range.
pub fn run_blame(global: &GlobalArgs, args: &BlameArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let mut rev: Option<String> = None;
    let file: String;
    match args.args.as_slice() {
        [file_arg] => file = file_arg.clone(),
        [rev_arg, file_arg] => {
            rev = Some(rev_arg.clone());
            file = file_arg.clone();
        }
        _ => {
            eprintln!("usage: git blame [<options>] [<rev-opts>] [<rev>] [--] <file>");
            return Ok(129);
        }
    }
    let path = super::worktree_path(&repository, std::path::Path::new(&file), "blame")?;
    let tip = match &rev {
        Some(spec) => match super::resolve_commit(&repository, spec) {
            Ok(oid) => oid,
            Err(_) => {
                eprintln!("fatal: bad revision '{spec}'");
                return Ok(128);
            }
        },
        None => match repository.head()?.oid() {
            Some(oid) => oid,
            None => {
                eprintln!("fatal: no such ref: HEAD");
                return Ok(128);
            }
        },
    };
    let tip_tree = repository.commit_tree(tip)?;
    let tip_map = rivet_core::ops::flatten_tree(repository.odb(), tip_tree)?;
    let Some((_, committed_blob)) = tip_map.get(BStr::new(&path)).copied() else {
        let shown = String::from_utf8_lossy(&path);
        match &rev {
            Some(spec) => eprintln!("fatal: no such path {shown} in {spec}"),
            None => eprintln!("fatal: no such path '{shown}' in HEAD"),
        }
        return Ok(128);
    };
    let committed = repository
        .odb()
        .read_typed(&committed_blob, ObjectKind::Blob)?
        .into_data();

    // Without a revision the work tree's version is blamed, so uncommitted lines show up.
    let (content, top_is_worktree) = if rev.is_none() {
        match repository.worktree()?.read_file(BStr::new(&path[..])) {
            Ok(data) => {
                let changed = data != committed;
                (data, changed)
            }
            Err(_) => {
                eprintln!("fatal: Cannot lstat '{file}': No such file or directory");
                return Ok(128);
            }
        }
    } else {
        (committed.clone(), false)
    };
    let lines = split_lines(&content);
    let total = lines.len();

    let options = Options {
        ignore_ws: args.ignore_whitespace,
        mov: args.detect_moves || args.detect_copies > 0,
        copy: args.detect_copies > 0,
        harder: args.detect_copies > 1,
        hardest: args.detect_copies > 2,
        move_score: args.move_score.last().copied().unwrap_or(20),
        copy_score: args.copy_score.last().copied().unwrap_or(40),
    };
    if top_is_worktree && (options.mov || options.copy) {
        anyhow::bail!("blame: -M/-C with uncommitted changes in the file is not implemented");
    }

    let mut wanted = vec![false; total];
    if args.ranges.is_empty() {
        wanted.fill(true);
    }
    let text_lines: Vec<&[u8]> = lines
        .iter()
        .map(|line| line.content.strip_suffix(b"\n").unwrap_or(line.content))
        .collect();
    let mut anchor = 1;
    for range in &args.ranges {
        if range.starts_with(':') || range.starts_with("^:") || range.contains(":/") {
            anyhow::bail!("blame: function-name -L ranges are not implemented");
        }
        let parsed = if range.contains('/') {
            parse_regex_range(range, &text_lines, anchor, &file)
        } else {
            parse_range(range, total, &file).map_err(|error| RangeError::Fatal(error.to_string()))
        };
        match parsed {
            Ok((start, end)) => {
                for flag in &mut wanted[start - 1..end] {
                    *flag = true;
                }
                anchor = end + 1;
            }
            Err(RangeError::Fatal(message)) => {
                eprintln!("fatal: {message}");
                return Ok(128);
            }
            Err(RangeError::Usage) => {
                eprintln!("usage: git blame [<options>] [<rev-opts>] [<rev>] [--] <file>");
                return Ok(129);
            }
        }
    }

    let mut line_starts = Vec::with_capacity(total + 1);
    let mut offset = 0;
    for line in &lines {
        line_starts.push(offset);
        offset += line.content.len();
    }
    line_starts.push(offset);
    let final_data = content.clone();

    let mut blamer = Blamer {
        repo: &repository,
        final_data,
        line_starts,
        origins: Vec::new(),
        by_key: HashMap::new(),
        commit_origins: HashMap::new(),
        queue: BinaryHeap::new(),
        in_queue: HashSet::new(),
        counter: 0,
        dates: HashMap::new(),
        trees: HashMap::new(),
        tip,
        tip_tree,
        final_entries: Vec::new(),
        guilty: Vec::new(),
        options,
    };
    let first = if top_is_worktree {
        blamer.get_origin(None, &path, ObjectId::zero(repository.format()))?
    } else {
        blamer.get_origin(Some(tip), &path, committed_blob)?
    };
    if top_is_worktree {
        blamer.origins[first].data = Rc::new(content.clone());
    }
    // One entry per run of wanted lines.
    let mut run_start = None;
    let mut initial: Vec<Entry> = Vec::new();
    for (i, &on) in wanted.iter().chain(std::iter::once(&false)).enumerate() {
        match (run_start, on) {
            (None, true) => run_start = Some(i),
            (Some(start), false) => {
                initial.push(Entry {
                    lno: start,
                    num: i - start,
                    suspect: first,
                    s_lno: start,
                });
                run_start = None;
            }
            _ => {}
        }
    }
    blamer.origins[first].suspects = initial;
    blamer.assign_blame(first)?;

    // The origin of every shown line.
    let shown: Vec<usize> = (0..total).filter(|i| wanted[*i]).collect();
    let mut line_entry: Vec<Option<usize>> = vec![None; total];
    for (index, e) in blamer.final_entries.iter().enumerate() {
        for slot in &mut line_entry[e.lno..e.lno + e.num] {
            *slot = Some(index);
        }
    }
    let origin_of = |i: usize| -> &Origin {
        let entry = &blamer.final_entries[line_entry[i].expect("every line is blamed")];
        &blamer.origins[entry.suspect]
    };

    let mut people: HashMap<Option<ObjectId>, Person> = HashMap::new();
    for &i in &shown {
        let origin = origin_of(i);
        if people.contains_key(&origin.commit) {
            continue;
        }
        let person = match origin.commit {
            Some(oid) => {
                let object = repository.odb().read_typed(&oid, ObjectKind::Commit)?;
                let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
                Person {
                    name: String::from_utf8_lossy(&commit.author.name).into_owned(),
                    email: format!("<{}>", String::from_utf8_lossy(&commit.author.email)),
                    when: commit.author.when,
                    tz: commit.author.tz_offset_minutes,
                    committer: String::from_utf8_lossy(&commit.committer.name).into_owned(),
                    committer_email: format!(
                        "<{}>",
                        String::from_utf8_lossy(&commit.committer.email)
                    ),
                    committer_when: commit.committer.when,
                    committer_tz: commit.committer.tz_offset_minutes,
                    summary: String::from_utf8_lossy(
                        commit.message.split(|b| *b == b'\n').next().unwrap_or(b""),
                    )
                    .into_owned(),
                }
            }
            None => {
                // Git stamps the fake work-tree commit with the current time in the local zone.
                let (when, tz) =
                    rivet_core::object::commit::parse_git_date(BStr::new(b"now")).unwrap_or((0, 0));
                Person {
                    name: "Not Committed Yet".to_string(),
                    email: "<not.committed.yet>".to_string(),
                    when,
                    tz,
                    committer: "Not Committed Yet".to_string(),
                    committer_email: "<not.committed.yet>".to_string(),
                    committer_when: when,
                    committer_tz: tz,
                    summary: format!(
                        "Version of {} from {}",
                        String::from_utf8_lossy(&path),
                        file
                    ),
                }
            }
        };
        people.insert(origin.commit, person);
    }

    let mut out: Vec<u8> = Vec::new();
    if args.incremental {
        render_incremental(&blamer, &people, &mut out);
    } else if args.porcelain || args.line_porcelain {
        render_porcelain(
            &blamer,
            &lines,
            &wanted,
            &people,
            args.line_porcelain,
            &mut out,
        );
    } else {
        let any_boundary = shown.iter().any(|&i| origin_of(i).boundary);
        let abbrev = if args.long {
            repository.format().hex_len()
        } else if any_boundary {
            8
        } else {
            7
        };
        let name_of = |commit: &Option<ObjectId>| -> String {
            let person = &people[commit];
            if args.email {
                person.email.clone()
            } else {
                person.name.clone()
            }
        };
        let name_width = shown
            .iter()
            .map(|&i| name_of(&origin_of(i).commit).chars().count())
            .max()
            .unwrap_or(0);
        let number_width = shown.last().map(|n| (n + 1).to_string().len()).unwrap_or(1);
        let show_file = args.show_name || shown.iter().any(|&i| origin_of(i).path != path[..]);
        let file_width = shown
            .iter()
            .map(|&i| origin_of(i).path.len())
            .max()
            .unwrap_or(0);
        for &i in &shown {
            let origin = origin_of(i);
            let sha = match origin.commit {
                Some(oid) => {
                    let hex = oid.to_string();
                    if origin.boundary {
                        format!("^{}", &hex[..abbrev - 1])
                    } else {
                        hex[..abbrev].to_string()
                    }
                }
                None => "0".repeat(abbrev),
            };
            out.extend_from_slice(sha.as_bytes());
            if show_file {
                out.push(b' ');
                out.extend_from_slice(&origin.path);
                out.extend_from_slice(&vec![b' '; file_width - origin.path.len()]);
            }
            out.push(b' ');
            if !args.suppress {
                out.push(b'(');
                let person = &people[&origin.commit];
                let name = name_of(&origin.commit);
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(" ".repeat(name_width - name.chars().count()).as_bytes());
                out.push(b' ');
                let date =
                    rivet_core::ops::format_date_style("iso", person.when, person.tz, person.when)
                        .unwrap_or_default();
                out.extend_from_slice(date.as_bytes());
                out.push(b' ');
            }
            out.extend_from_slice(format!("{:>width$}) ", i + 1, width = number_width).as_bytes());
            out.extend_from_slice(lines[i].content);
            if !lines[i].has_newline {
                out.push(b'\n');
            }
        }
    }
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&out).and_then(|_| stdout.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    Ok(0)
}

/// What the output needs to know about a commit.
struct Person {
    name: String,
    email: String,
    when: i64,
    tz: i32,
    committer: String,
    committer_email: String,
    committer_when: i64,
    committer_tz: i32,
    summary: String,
}

fn tz_text(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.abs();
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

/// `--incremental`: per settled group a header, the commit's details the first time it appears,
/// then `previous` and `filename`; the lines themselves are not shown.
fn render_incremental(
    blamer: &Blamer,
    people: &HashMap<Option<ObjectId>, Person>,
    out: &mut Vec<u8>,
) {
    let mut shown: HashSet<Option<ObjectId>> = HashSet::new();
    for e in &blamer.guilty {
        let origin = &blamer.origins[e.suspect];
        let hex = match origin.commit {
            Some(oid) => oid.to_string(),
            None => "0".repeat(blamer.repo.format().hex_len()),
        };
        out.extend_from_slice(
            format!("{hex} {} {} {}\n", e.s_lno + 1, e.lno + 1, e.num).as_bytes(),
        );
        if shown.insert(origin.commit) {
            let person = &people[&origin.commit];
            for (key, value) in [
                ("author", person.name.clone()),
                ("author-mail", person.email.clone()),
                ("author-time", person.when.to_string()),
                ("author-tz", tz_text(person.tz)),
                ("committer", person.committer.clone()),
                ("committer-mail", person.committer_email.clone()),
                ("committer-time", person.committer_when.to_string()),
                ("committer-tz", tz_text(person.committer_tz)),
                ("summary", person.summary.clone()),
            ] {
                out.extend_from_slice(format!("{key} {value}\n").as_bytes());
            }
            if origin.boundary {
                out.extend_from_slice(b"boundary\n");
            }
        }
        if let Some((parent, path)) = &origin.previous {
            out.extend_from_slice(format!("previous {parent} ").as_bytes());
            out.extend_from_slice(path);
            out.push(b'\n');
        }
        out.extend_from_slice(b"filename ");
        out.extend_from_slice(&origin.path);
        out.push(b'\n');
    }
}

/// `--porcelain` / `--line-porcelain`: a header per group of lines, the commit's details the
/// first time it appears (every line with `--line-porcelain`), then the line behind a tab.
fn render_porcelain(
    blamer: &Blamer,
    lines: &[rivet_core::diff::myers::DiffLine],
    wanted: &[bool],
    people: &HashMap<Option<ObjectId>, Person>,
    repeat: bool,
    out: &mut Vec<u8>,
) {
    let mut shown: HashSet<Option<ObjectId>> = HashSet::new();
    // A commit blamed under several paths names the file for every group.
    let mut paths_of: HashMap<Option<ObjectId>, HashSet<&[u8]>> = HashMap::new();
    for e in &blamer.final_entries {
        let origin = &blamer.origins[e.suspect];
        paths_of
            .entry(origin.commit)
            .or_default()
            .insert(&origin.path);
    }
    for e in &blamer.final_entries {
        let origin = &blamer.origins[e.suspect];
        let hex = match origin.commit {
            Some(oid) => oid.to_string(),
            None => "0".repeat(blamer.repo.format().hex_len()),
        };
        let person = &people[&origin.commit];
        let details = |out: &mut Vec<u8>, shown: &mut HashSet<Option<ObjectId>>| {
            let first_time = shown.insert(origin.commit);
            let mut wrote = false;
            if repeat || first_time {
                wrote = true;
                for (key, value) in [
                    ("author", person.name.clone()),
                    ("author-mail", person.email.clone()),
                    ("author-time", person.when.to_string()),
                    ("author-tz", tz_text(person.tz)),
                    ("committer", person.committer.clone()),
                    ("committer-mail", person.committer_email.clone()),
                    ("committer-time", person.committer_when.to_string()),
                    ("committer-tz", tz_text(person.committer_tz)),
                    ("summary", person.summary.clone()),
                ] {
                    out.extend_from_slice(format!("{key} {value}\n").as_bytes());
                }
                if origin.boundary {
                    out.extend_from_slice(b"boundary\n");
                }
            }
            if wrote || paths_of[&origin.commit].len() > 1 {
                if let Some((parent, path)) = &origin.previous {
                    out.extend_from_slice(format!("previous {parent} ").as_bytes());
                    out.extend_from_slice(path);
                    out.push(b'\n');
                }
                out.extend_from_slice(b"filename ");
                out.extend_from_slice(&origin.path);
                out.push(b'\n');
            }
        };
        for count in 0..e.num {
            let line = e.lno + count;
            if !wanted[line] {
                continue;
            }
            if count == 0 {
                out.extend_from_slice(
                    format!("{hex} {} {} {}\n", e.s_lno + 1, e.lno + 1, e.num).as_bytes(),
                );
                details(out, &mut shown);
            } else {
                out.extend_from_slice(
                    format!("{hex} {} {}\n", e.s_lno + 1 + count, line + 1).as_bytes(),
                );
                if repeat {
                    details(out, &mut shown);
                }
            }
            out.push(b'\t');
            out.extend_from_slice(lines[line].content);
            if !lines[line].has_newline {
                out.push(b'\n');
            }
        }
    }
}
