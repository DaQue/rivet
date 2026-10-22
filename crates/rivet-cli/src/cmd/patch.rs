//! Parsing and applying unified diffs (`git diff`, `git format-patch`, `diff -u`), the engine
//! behind `apply` and `am`.
//!
//! # Rules taken from Git
//!
//! * A hunk applies where its pre-image (context and removed lines) matches the file exactly,
//!   looked for at the stated position first and then at growing offsets, backwards before
//!   forwards. There is no fuzz: every context line must match.
//! * A hunk that starts at line 1 must match at the start of the file, and a hunk with no trailing
//!   context must match at the end (unless `--unidiff-zero`).
//! * `\ No newline at end of file` decides whether the result ends in a newline.
//! * A patch is applied to every file or to none.
//!
//! A binary patch applies only when its `index` line has full ids and the post-image blob is in
//! the object database; the data of a `GIT binary patch`, whitespace fixing and fuzz are not
//! implemented and are refused.

use anyhow::{Result, bail};

#[derive(Clone, Debug, Default)]
pub struct Hunk {
    pub old_start: usize,
    pub new_start: usize,
    /// `(' ' | '-' | '+', content without the newline)`
    pub lines: Vec<(u8, Vec<u8>)>,
    /// The pre-image ends without a newline (`\ No newline` after a ` ` or `-` line).
    pub old_no_newline: bool,
    /// The post-image ends without a newline.
    pub new_no_newline: bool,
    /// The hunk exactly as the patch has it (`@@` line to the last line, `\ No newline`
    /// markers included, every line newline-terminated): what a `.rej` file repeats.
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct FilePatch {
    pub old_path: Option<Vec<u8>>,
    pub new_path: Option<Vec<u8>>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub is_new: bool,
    pub is_delete: bool,
    pub rename: bool,
    pub copy: bool,
    pub hunks: Vec<Hunk>,
    /// The abbreviated blob ids of the `index <old>..<new>` line.
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    /// The `similarity index` of a rename or copy.
    pub similarity: Option<u32>,
    /// A binary patch: `Binary files … differ` (no data) or `GIT binary patch` (`binary_data`).
    pub binary: bool,
    pub binary_data: bool,
    pub lines_added: usize,
    pub lines_deleted: usize,
}

fn unquote(name: &[u8]) -> Vec<u8> {
    if name.len() >= 2 && name[0] == b'"' && name[name.len() - 1] == b'"' {
        let inner = &name[1..name.len() - 1];
        let mut out = Vec::new();
        let mut i = 0;
        while i < inner.len() {
            if inner[i] == b'\\' && i + 1 < inner.len() {
                i += 1;
                match inner[i] {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    d @ b'0'..=b'7' => {
                        let mut value = (d - b'0') as u32;
                        let mut used = 0;
                        while used < 2
                            && i + 1 < inner.len()
                            && (b'0'..=b'7').contains(&inner[i + 1])
                        {
                            i += 1;
                            value = value * 8 + (inner[i] - b'0') as u32;
                            used += 1;
                        }
                        out.push(value as u8);
                    }
                    other => out.push(other),
                }
            } else {
                out.push(inner[i]);
            }
            i += 1;
        }
        out
    } else {
        name.to_vec()
    }
}

fn strip(name: &[u8], strip: usize) -> Option<Vec<u8>> {
    let mut rest = name;
    for _ in 0..strip {
        let slash = rest.iter().position(|b| *b == b'/')?;
        rest = &rest[slash + 1..];
    }
    Some(rest.to_vec())
}

/// A name from a `---`/`+++` line: up to a tab, quotes undone, prefix stripped; `None` for
/// `/dev/null`.
fn header_name(text: &[u8], strip_n: usize) -> Result<Option<Vec<u8>>> {
    let text = text.split(|b| *b == b'\t').next().unwrap_or(text);
    let text = text.strip_suffix(b"\r").unwrap_or(text);
    if text == b"/dev/null" {
        return Ok(None);
    }
    let name = unquote(text);
    match strip(&name, strip_n) {
        Some(stripped) => Ok(Some(stripped)),
        None => bail!(
            "git diff header lacks filename information when removing {strip_n} leading pathname component(s)"
        ),
    }
}

fn parse_range(text: &[u8]) -> Option<(usize, usize)> {
    let text = std::str::from_utf8(text).ok()?;
    match text.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

fn parse_hunk_header(line: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix(b"@@ -")?;
    let end = rest.windows(3).position(|w| w == b" @@")?;
    let body = &rest[..end];
    let plus = body.windows(2).position(|w| w == b" +")?;
    let (old_start, old_len) = parse_range(&body[..plus])?;
    let (new_start, new_len) = parse_range(&body[plus + 2..])?;
    Some((old_start, old_len, new_start, new_len))
}

fn mark_no_newline(hunk: &mut Hunk) {
    match hunk.lines.last().map(|(kind, _)| *kind) {
        Some(b'-') => hunk.old_no_newline = true,
        Some(b'+') => hunk.new_no_newline = true,
        Some(_) => {
            hunk.old_no_newline = true;
            hunk.new_no_newline = true;
        }
        None => {}
    }
}

/// Parse every file patch in `text`, stripping `strip_n` leading path components.
///
/// # Errors
///
/// A malformed hunk, a binary patch, or a header without a file name.
pub fn parse(text: &[u8], strip_n: usize) -> Result<Vec<FilePatch>> {
    let lines: Vec<&[u8]> = text.split(|b| *b == b'\n').collect();
    let mut patches: Vec<FilePatch> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let git_header = line.starts_with(b"diff --git ");
        let plain_header = line.starts_with(b"--- ")
            && lines
                .get(i + 1)
                .is_some_and(|next| next.starts_with(b"+++ "));
        if !git_header && !plain_header {
            i += 1;
            continue;
        }
        let mut patch = FilePatch::default();
        let mut names_from_git: Option<(Vec<u8>, Vec<u8>)> = None;
        if git_header {
            // `diff --git a/x b/x`: the names are only a fallback when there is no ---/+++.
            let rest = &line[11..];
            let mut halves: Option<(&[u8], &[u8])> = None;
            for (at, byte) in rest.iter().enumerate() {
                if *byte == b' ' {
                    let (left, right) = (&rest[..at], &rest[at + 1..]);
                    if strip(&unquote(left), strip_n) == strip(&unquote(right), strip_n)
                        && strip(&unquote(left), strip_n).is_some()
                    {
                        halves = Some((left, right));
                        break;
                    }
                }
            }
            if halves.is_none()
                && let Some(mid) = rest.windows(3).position(|w| w == b" b/")
            {
                halves = Some((&rest[..mid], &rest[mid + 1..]));
            }
            if let Some((left, right)) = halves {
                names_from_git = Some((
                    strip(&unquote(left), strip_n).unwrap_or_default(),
                    strip(&unquote(right), strip_n).unwrap_or_default(),
                ));
            }
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                if let Some(v) = l.strip_prefix(b"old mode ") {
                    patch.old_mode = u32::from_str_radix(&String::from_utf8_lossy(v), 8).ok();
                } else if let Some(v) = l.strip_prefix(b"new mode ") {
                    patch.new_mode = u32::from_str_radix(&String::from_utf8_lossy(v), 8).ok();
                } else if let Some(v) = l.strip_prefix(b"new file mode ") {
                    patch.is_new = true;
                    patch.new_mode = u32::from_str_radix(&String::from_utf8_lossy(v), 8).ok();
                } else if let Some(v) = l.strip_prefix(b"deleted file mode ") {
                    patch.is_delete = true;
                    patch.old_mode = u32::from_str_radix(&String::from_utf8_lossy(v), 8).ok();
                } else if let Some(v) = l.strip_prefix(b"rename from ") {
                    patch.rename = true;
                    patch.old_path = Some(unquote(v));
                } else if let Some(v) = l.strip_prefix(b"rename to ") {
                    patch.new_path = Some(unquote(v));
                } else if let Some(v) = l.strip_prefix(b"copy from ") {
                    patch.copy = true;
                    patch.old_path = Some(unquote(v));
                } else if let Some(v) = l.strip_prefix(b"copy to ") {
                    patch.new_path = Some(unquote(v));
                } else if let Some(v) = l.strip_prefix(b"similarity index ") {
                    patch.similarity = String::from_utf8_lossy(v)
                        .trim_end_matches('%')
                        .parse()
                        .ok();
                } else if l.starts_with(b"dissimilarity index ") {
                } else if let Some(v) = l.strip_prefix(b"index ") {
                    let text = String::from_utf8_lossy(v);
                    let ids = text.split(' ').next().unwrap_or("");
                    if let Some((old, new)) = ids.split_once("..") {
                        patch.old_oid = Some(old.to_string());
                        patch.new_oid = Some(new.to_string());
                    }
                } else if l.starts_with(b"GIT binary patch") {
                    patch.binary = true;
                    patch.binary_data = true;
                    i += 1;
                    // Two blocks (forward, reverse), each ended by a blank line.
                    let mut blanks = 0;
                    while i < lines.len() && !lines[i].starts_with(b"diff --git ") {
                        if lines[i].is_empty() {
                            blanks += 1;
                            if blanks == 2 {
                                i += 1;
                                break;
                            }
                        }
                        i += 1;
                    }
                    break;
                } else if l.starts_with(b"Binary files ") {
                    patch.binary = true;
                    i += 1;
                    break;
                } else {
                    break;
                }
                i += 1;
            }
        }
        // ---/+++ lines.
        if i + 1 < lines.len() && lines[i].starts_with(b"--- ") && lines[i + 1].starts_with(b"+++ ")
        {
            let old = header_name(&lines[i][4..], strip_n)?;
            let new = header_name(&lines[i + 1][4..], strip_n)?;
            if old.is_none() {
                patch.is_new = true;
            }
            if new.is_none() {
                patch.is_delete = true;
            }
            if !patch.rename && !patch.copy {
                patch.old_path = old;
                patch.new_path = new;
            }
            i += 2;
        } else if let Some((old, new)) = names_from_git {
            // A pure mode change or a rename without content has no ---/+++ lines.
            if patch.old_path.is_none() {
                patch.old_path = Some(old);
            }
            if patch.new_path.is_none() {
                patch.new_path = Some(new);
            }
        }
        if patch.old_path.is_none() && !patch.is_new {
            patch.old_path = patch.new_path.clone();
        }
        if patch.new_path.is_none() && !patch.is_delete {
            patch.new_path = patch.old_path.clone();
        }
        // Hunks.
        while i < lines.len() {
            let Some((old_start, old_len, new_start, new_len)) = parse_hunk_header(lines[i]) else {
                break;
            };
            let hunk_first = i;
            i += 1;
            let mut hunk = Hunk {
                old_start,
                new_start,
                ..Hunk::default()
            };
            let (mut old_seen, mut new_seen) = (0usize, 0usize);
            while (old_seen < old_len || new_seen < new_len) && i < lines.len() {
                let l = lines[i];
                if l.starts_with(b"\\") {
                    mark_no_newline(&mut hunk);
                    i += 1;
                    continue;
                }
                match l.first() {
                    Some(b' ') | None if l.is_empty() || l[0] == b' ' => {
                        // An empty line inside a hunk is an empty context line.
                        hunk.lines.push((b' ', l.get(1..).unwrap_or(&[]).to_vec()));
                        old_seen += 1;
                        new_seen += 1;
                    }
                    Some(b'-') => {
                        hunk.lines.push((b'-', l[1..].to_vec()));
                        old_seen += 1;
                    }
                    Some(b'+') => {
                        hunk.lines.push((b'+', l[1..].to_vec()));
                        new_seen += 1;
                    }
                    _ => bail!("corrupt patch at line {}", i + 1),
                }
                i += 1;
            }
            while i < lines.len() && lines[i].starts_with(b"\\") {
                mark_no_newline(&mut hunk);
                i += 1;
            }
            if old_seen != old_len || new_seen != new_len {
                bail!("corrupt patch at line {i}");
            }
            for line in &lines[hunk_first..i] {
                hunk.raw.extend_from_slice(line);
                hunk.raw.push(b'\n');
            }
            patch.hunks.push(hunk);
        }
        for hunk in &patch.hunks {
            for (kind, _) in &hunk.lines {
                match kind {
                    b'+' => patch.lines_added += 1,
                    b'-' => patch.lines_deleted += 1,
                    _ => {}
                }
            }
        }
        patches.push(patch);
    }
    Ok(patches)
}

/// A file split into lines, remembering whether the last one ends in a newline.
struct Content {
    lines: Vec<Vec<u8>>,
    trailing_newline: bool,
}

fn split_content(data: &[u8]) -> Content {
    if data.is_empty() {
        return Content {
            lines: Vec::new(),
            trailing_newline: true,
        };
    }
    let mut lines: Vec<Vec<u8>> = data.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let trailing_newline = data.ends_with(b"\n");
    if trailing_newline {
        lines.pop();
    }
    Content {
        lines,
        trailing_newline,
    }
}

fn join_content(content: &Content) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in content.lines.iter().enumerate() {
        out.extend_from_slice(line);
        if i + 1 < content.lines.len() || content.trailing_newline {
            out.push(b'\n');
        }
    }
    out
}

/// Why a hunk did not apply: the 1-based line the hunk claimed.
#[derive(Debug)]
pub struct HunkFailure {
    pub line: usize,
    /// The pre-image the hunk was looked for as (what git prints after `while searching for:`).
    pub searched: Vec<u8>,
    /// 1-based position of the hunk in its file patch.
    pub number: usize,
}

/// Apply `hunks` to `data`; `reverse` swaps the two sides.
///
/// Returns the data with every hunk that fit applied, and the hunks that did not. Without
/// `keep_going` it stops at the first failure (the data is then incomplete). `--reject` goes on.
pub fn apply_hunks_collect(
    data: &[u8],
    hunks: &[Hunk],
    reverse: bool,
    unidiff_zero: bool,
    keep_going: bool,
) -> (Vec<u8>, Vec<HunkFailure>) {
    let mut content = split_content(data);
    let mut drift: isize = 0;
    let mut failures = Vec::new();
    for (number, hunk) in hunks.iter().enumerate() {
        // Pre- and post-image of the hunk (swapped when reversing).
        let (pre_kind, post_kind) = if reverse { (b'+', b'-') } else { (b'-', b'+') };
        let pre: Vec<&[u8]> = hunk
            .lines
            .iter()
            .filter(|(k, _)| *k == b' ' || *k == pre_kind)
            .map(|(_, l)| l.as_slice())
            .collect();
        let post: Vec<Vec<u8>> = hunk
            .lines
            .iter()
            .filter(|(k, _)| *k == b' ' || *k == post_kind)
            .map(|(_, l)| l.clone())
            .collect();
        let (old_start, new_no_newline, old_no_newline) = if reverse {
            (hunk.new_start, hunk.old_no_newline, hunk.new_no_newline)
        } else {
            (hunk.old_start, hunk.new_no_newline, hunk.old_no_newline)
        };
        let trailing = hunk
            .lines
            .iter()
            .rev()
            .take_while(|(k, _)| *k == b' ')
            .count();
        let match_beginning = !unidiff_zero && (old_start == 0 || old_start == 1);
        let match_end = !unidiff_zero && trailing == 0;
        let expected = if pre.is_empty() {
            // A pure addition: after line `old_start` (0 means the very start).
            old_start as isize
        } else {
            old_start as isize - 1
        };
        let wanted = (expected + drift).max(0) as usize;
        let fits = |at: usize| -> bool {
            if at + pre.len() > content.lines.len() {
                return false;
            }
            if match_beginning && at != 0 {
                return false;
            }
            if match_end && at + pre.len() != content.lines.len() {
                return false;
            }
            for (k, line) in pre.iter().enumerate() {
                if content.lines[at + k] != *line {
                    return false;
                }
            }
            // The pre-image's last line must agree about its newline.
            if at + pre.len() == content.lines.len()
                && !pre.is_empty()
                && old_no_newline == content.trailing_newline
            {
                return false;
            }
            true
        };
        let mut found: Option<usize> = None;
        let max = content.lines.len() + 1;
        for distance in 0..=max {
            if distance <= wanted && fits(wanted - distance) {
                found = Some(wanted - distance);
                break;
            }
            if distance > 0 && wanted + distance <= content.lines.len() && fits(wanted + distance) {
                found = Some(wanted + distance);
                break;
            }
        }
        let Some(at) = found else {
            let mut searched = Vec::new();
            for line in &pre {
                searched.extend_from_slice(line);
                searched.push(b'\n');
            }
            if old_no_newline && !pre.is_empty() {
                searched.pop();
            }
            failures.push(HunkFailure {
                line: old_start.max(1),
                searched,
                number: number + 1,
            });
            if keep_going {
                continue;
            }
            break;
        };
        drift = at as isize - expected;
        let ends_here = at + pre.len() == content.lines.len();
        content.lines.splice(at..at + pre.len(), post);
        if ends_here {
            content.trailing_newline = !new_no_newline;
        }
    }
    (join_content(&content), failures)
}

/// What applying a set of patches to a repository needs to know.
#[derive(Clone, Copy, Default)]
pub struct ApplyOptions {
    pub reverse: bool,
    /// Update the index as well as the work tree.
    pub index: bool,
    /// Update only the index.
    pub cached: bool,
    /// Check that the patches apply and change nothing.
    pub check: bool,
    pub unidiff_zero: bool,
    /// Keep going after a hunk that does not fit: write it to `<path>.rej` (`--reject`).
    pub reject: bool,
    /// Merge the patch into the current content when it does not apply by content (`--3way`).
    pub three_way: bool,
}

use bstr::{BStr, BString};
use rivet_core::object::EntryMode;
use rivet_core::{ObjectId, ObjectKind, Repository};

/// Apply `patches` to `repository`. On failure nothing has been written and the returned lines
/// are the diagnostics Git prints (`error: …`).
pub fn apply_to_repo(
    repository: &mut Repository,
    patches: &[FilePatch],
    options: ApplyOptions,
) -> std::result::Result<Vec<String>, Vec<String>> {
    use std::collections::BTreeMap;
    // Pending state per path: `Some((data, mode))` once written, `None` once removed.
    let mut pending: BTreeMap<Vec<u8>, Option<(Vec<u8>, EntryMode)>> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let show = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
    // `--reject`: git narrates every step; `errors` is then the ordered stderr transcript of the
    // check phase, `soft` counts its lines that only report a rejected hunk, `applied` is what the
    // apply phase says per patch, `rejects` the `.rej` files to write.
    let mut soft = 0usize;
    let mut applied: Vec<String> = Vec::new();
    let mut rejects: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut rejected_hunks = 0usize;
    let mut conflicts: Vec<(Vec<u8>, ThreeWayMerge)> = Vec::new();

    // `-R` goes through the patches last to first, as git does.
    let ordered: Vec<&FilePatch> = if options.reverse {
        patches.iter().rev().collect()
    } else {
        patches.iter().collect()
    };
    for patch in ordered {
        let name = |n: &Option<Vec<u8>>| n.as_deref().map(show);
        let (said_old, said_new) = if options.reverse {
            (&patch.new_path, &patch.old_path)
        } else {
            (&patch.old_path, &patch.new_path)
        };
        let said = match (name(said_old), name(said_new)) {
            (Some(old), Some(new)) if old != new => format!("{old} => {new}"),
            (_, Some(new)) => new,
            (Some(old), None) => old,
            (None, None) => String::new(),
        };
        if options.reject {
            errors.push(format!("Checking patch {said}..."));
        }
        let (old, new) = if options.reverse {
            (patch.new_path.clone(), patch.old_path.clone())
        } else {
            (patch.old_path.clone(), patch.new_path.clone())
        };
        let (is_new, is_delete) = if options.reverse {
            (patch.is_delete, patch.is_new)
        } else {
            (patch.is_new, patch.is_delete)
        };
        let (old_mode, new_mode) = if options.reverse {
            (patch.new_mode, patch.old_mode)
        } else {
            (patch.old_mode, patch.new_mode)
        };
        let read = |repository: &Repository,
                    pending: &BTreeMap<Vec<u8>, Option<(Vec<u8>, EntryMode)>>,
                    path: &[u8]|
         -> Option<(Vec<u8>, EntryMode)> {
            if let Some(state) = pending.get(path) {
                return state.clone();
            }
            if options.cached {
                let entry = repository.index().entry(BStr::new(path))?;
                let data = repository
                    .odb()
                    .read_typed(&entry.oid, ObjectKind::Blob)
                    .ok()?
                    .into_data();
                return Some((data, entry.mode));
            }
            let worktree = repository.worktree().ok()?;
            let stat = worktree.stat(BStr::new(path)).ok()??;
            if stat.mode == EntryMode::Tree {
                return None;
            }
            Some((worktree.read_file(BStr::new(path)).ok()?, stat.mode))
        };

        let source = if is_new {
            None
        } else {
            let Some(path) = &old else {
                errors.push("error: patch without a file name".to_string());
                continue;
            };
            match read(repository, &pending, path) {
                Some(found) => Some((path.clone(), found)),
                None => {
                    errors.push(format!(
                        "error: {}: {}",
                        show(path),
                        if options.cached || options.index {
                            "does not exist in index"
                        } else {
                            "No such file or directory"
                        }
                    ));
                    continue;
                }
            }
        };
        if options.index
            && let Some((path, (data, _))) = &source
            && !pending.contains_key(path)
        {
            let matches_index = repository.index().entry(BStr::new(path)).is_some_and(|e| {
                repository
                    .odb()
                    .read_typed(&e.oid, ObjectKind::Blob)
                    .is_ok_and(|o| o.data() == data.as_slice())
            });
            if !matches_index {
                errors.push(format!("error: {}: does not match index", show(path)));
                continue;
            }
        }
        let Some(target) = new.clone().or_else(|| old.clone()) else {
            errors.push("error: patch without a file name".to_string());
            continue;
        };
        // With `--3way`, creating a file that exists is not an error: the existing content is
        // merged with the new file as if both had been added.
        let mut existing: Option<Vec<u8>> = None;
        if is_new && let Some((data, _)) = read(repository, &pending, &target) {
            if options.three_way {
                existing = Some(data);
            } else {
                errors.push(format!(
                    "error: {}: already exists in {}",
                    show(&target),
                    if options.cached {
                        "index"
                    } else {
                        "working directory"
                    }
                ));
                continue;
            }
        }
        let direct_to_threeway = existing.is_some();
        let (base_data, base_mode) = match (&source, existing) {
            (Some((_, (data, mode))), _) => (data.clone(), *mode),
            (None, Some(data)) => (data, EntryMode::Blob),
            (None, None) => (Vec::new(), EntryMode::Blob),
        };
        let mut merged: Option<ThreeWayMerge> = None;
        let mut direct = None;
        if patch.binary {
            match binary_result(repository, patch, options.reverse, &base_data) {
                Ok(data) => direct = Some((data, Vec::new())),
                Err(lines) => {
                    errors.extend(lines);
                    continue;
                }
            }
        } else if options.three_way {
            if direct_to_threeway {
                errors.push("Performing three-way merge...".to_string());
            }
            match three_way_merge(
                repository,
                patch,
                options,
                (is_new, is_delete, direct_to_threeway),
                &base_data,
            ) {
                Ok(done) => {
                    errors.push(if done.conflict {
                        format!("Applied patch to '{}' with conflicts.", show(&target))
                    } else {
                        format!("Applied patch to '{}' cleanly.", show(&target))
                    });
                    merged = Some(done);
                }
                Err(message) => {
                    // Git reports these with `error:` but goes on with a direct application.
                    soft += message.len();
                    errors.extend(message);
                    if !direct_to_threeway {
                        errors.push("Falling back to direct application...".to_string());
                    }
                }
            }
        }
        let (result, failures) = if let Some(done) = &merged {
            (done.data.clone(), Vec::new())
        } else if let Some(done) = direct {
            done
        } else {
            apply_hunks_collect(
                &base_data,
                &patch.hunks,
                options.reverse,
                options.unidiff_zero,
                options.reject,
            )
        };
        let shown_name = show(source.as_ref().map(|s| s.0.as_slice()).unwrap_or(&target));
        if !options.reject && !failures.is_empty() {
            errors.push(format!(
                "error: patch failed: {shown_name}:{}",
                failures[0].line
            ));
            errors.push(format!("error: {shown_name}: patch does not apply"));
            continue;
        }
        for failure in &failures {
            errors.push(format!(
                "error: while searching for:\n{}",
                String::from_utf8_lossy(&failure.searched)
            ));
            errors.push(format!(
                "error: patch failed: {shown_name}:{}",
                failure.line
            ));
            soft += 2;
        }
        let mut narrate = |target: &[u8]| {
            if !options.reject {
                return;
            }
            if failures.is_empty() {
                applied.push(format!("Applied patch {said} cleanly."));
                return;
            }
            let count = failures.len();
            applied.push(format!(
                "Applying patch {said} with {count} reject{}...",
                if count == 1 { "" } else { "s" }
            ));
            let mut text =
                format!("diff a/{0} b/{0}\t(rejected hunks)\n", show(target)).into_bytes();
            for number in 1..=patch.hunks.len() {
                if failures.iter().any(|f| f.number == number) {
                    applied.push(format!("Rejected hunk #{number}."));
                    text.extend_from_slice(&patch.hunks[number - 1].raw);
                } else {
                    applied.push(format!("Hunk #{number} applied cleanly."));
                }
            }
            rejected_hunks += count;
            let mut path = target.to_vec();
            path.extend_from_slice(b".rej");
            rejects.retain(|(name, _)| *name != path);
            rejects.push((path, text));
        };
        if let (Some(expected), Some((_, (_, actual)))) = (old_mode, &source) {
            let actual_bits = actual.to_u32();
            if actual_bits != expected && !is_new {
                let line = format!(
                    "warning: {} has type {actual_bits:o}, expected {expected:o}",
                    show(&target)
                );
                warnings.push(line.clone());
                errors.push(line);
            }
        }
        if is_delete {
            if !result.is_empty() {
                errors.push(format!(
                    "error: removal patch leaves file contents\nerror: {}: patch does not apply",
                    show(&target)
                ));
                continue;
            }
            narrate(&target);
            pending.insert(target, None);
            continue;
        }
        let mode = match new_mode {
            Some(0o100755) => EntryMode::BlobExecutable,
            Some(0o100644) => EntryMode::Blob,
            Some(0o120000) => {
                errors.push(format!(
                    "error: {}: symbolic links are not supported",
                    show(&target)
                ));
                continue;
            }
            _ => base_mode,
        };
        if patch.rename
            && let Some((from, _)) = &source
            && *from != target
        {
            pending.insert(from.clone(), None);
        }
        narrate(&target);
        if let Some(done) = merged
            && done.conflict
        {
            conflicts.push((target.clone(), done));
        }
        pending.insert(target, Some((result, mode)));
    }
    if options.reject {
        // Hunk failures are not errors here: the patch goes in with what fits.
        let hard = errors
            .iter()
            .filter(|line| line.starts_with("error:"))
            .count()
            - soft;
        if options.check {
            return if hard > 0 { Err(errors) } else { Ok(errors) };
        }
        errors.extend(std::mem::take(&mut applied));
        let failed = hard > 0 || rejected_hunks > 0;
        // A failed run leaves the index alone (but still writes the files and the `.rej`s).
        let update_index = !failed && (options.index || options.cached);
        if let Err(lines) = write_pending(
            repository,
            &pending,
            &conflicts,
            &rejects,
            options,
            update_index,
        ) {
            errors.extend(lines);
            return Err(errors);
        }
        return if failed { Err(errors) } else { Ok(errors) };
    }
    if options.three_way {
        // Only real errors stop the run; a conflict is written to the work tree and the index.
        let hard = errors
            .iter()
            .filter(|line| line.starts_with("error:"))
            .count()
            .saturating_sub(soft);
        if hard > 0 {
            return Err(errors);
        }
        if options.check {
            return Ok(errors);
        }
        for (path, _) in &conflicts {
            errors.push(format!("U {}", show(path)));
        }
        if let Err(lines) = write_pending(
            repository,
            &pending,
            &conflicts,
            &rejects,
            options,
            options.index || options.cached,
        ) {
            errors.extend(lines);
            return Err(errors);
        }
        return if conflicts.is_empty() {
            Ok(errors)
        } else {
            Err(errors)
        };
    }
    if errors.iter().any(|line| !line.starts_with("warning:")) {
        return Err(errors);
    }
    if options.check {
        return Ok(warnings);
    }
    write_pending(
        repository,
        &pending,
        &conflicts,
        &rejects,
        options,
        options.index || options.cached,
    )?;
    Ok(warnings)
}

/// The outcome of merging a patch into the current content (`--3way`).
struct ThreeWayMerge {
    data: Vec<u8>,
    conflict: bool,
    /// Stage 1 (absent for a new file), stage 2 (the current content), stage 3 (the post-image).
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
}

/// Git's `try_threeway`: build the pre-image from the `index` line, apply the patch to it, and
/// merge that post-image into `current`. `Err` carries the diagnostics before the fallback to
/// direct application (none when git falls back silently).
fn three_way_merge(
    repository: &Repository,
    patch: &FilePatch,
    options: ApplyOptions,
    (is_new, is_delete, direct): (bool, bool, bool),
    current: &[u8],
) -> std::result::Result<ThreeWayMerge, Vec<String>> {
    // Nothing to merge a deletion into, and a creation only merges into a file that exists.
    if is_delete || (is_new && !direct) {
        return Err(Vec::new());
    }
    let lacks =
        || vec!["error: repository lacks the necessary blob to perform 3-way merge.".to_string()];
    let odb = repository.odb();
    let format = odb.format();
    let abbreviation = if options.reverse {
        &patch.new_oid
    } else {
        &patch.old_oid
    };
    let (base, pre) = if is_new {
        // Git stores the empty blob it merges from.
        odb.write_raw(ObjectKind::Blob, b"")
            .map_err(|e| vec![format!("error: {e}")])?;
        (None, Vec::new())
    } else {
        let Some(hex) = abbreviation else {
            return Err(lacks());
        };
        // `from_hex_prefix` decodes whole bytes: an odd abbreviation loses its last digit.
        let even = hex.len() & !1;
        let Ok(prefix) = ObjectId::from_hex_prefix(format, BStr::new(&hex[..even])) else {
            return Err(lacks());
        };
        let Ok(oid) = odb.resolve_prefix(&prefix) else {
            return Err(lacks());
        };
        let Ok(object) = odb.read_typed(&oid, ObjectKind::Blob) else {
            return Err(lacks());
        };
        (Some(oid), object.into_data())
    };
    let (post, failures) = apply_hunks_collect(
        &pre,
        &patch.hunks,
        options.reverse,
        options.unidiff_zero,
        false,
    );
    if let Some(failure) = failures.first() {
        let (old_path, new_path) = if options.reverse {
            (&patch.new_path, &patch.old_path)
        } else {
            (&patch.old_path, &patch.new_path)
        };
        let name = old_path.as_ref().or(new_path.as_ref());
        return Err(vec![format!(
            "error: patch failed: {}:{}",
            String::from_utf8_lossy(name.map_or(&b""[..], |n| n)),
            failure.line
        )]);
    }
    let ours = odb
        .write_raw(ObjectKind::Blob, current)
        .map_err(|e| vec![format!("error: {e}")])?;
    let theirs = odb
        .write_raw(ObjectKind::Blob, &post)
        .map_err(|e| vec![format!("error: {e}")])?;
    let merged = rivet_core::merge::merge_content(
        Some(&pre),
        current,
        &post,
        ("ours", "theirs"),
        rivet_core::merge::ConflictStyle::Merge,
        rivet_core::diff::DiffAlgorithm::Myers,
    )
    .map_err(|e| vec![format!("error: {e}")])?;
    Ok(ThreeWayMerge {
        data: merged.data.to_vec(),
        conflict: merged.conflict,
        base,
        ours,
        theirs,
    })
}

/// The post-image of a binary patch. Only a `Binary files … differ` patch whose `index` line
/// carries full ids can be applied (the post-image blob must already be in the object database);
/// the data of a `GIT binary patch` (base85, zlib, delta) is not decoded.
fn binary_result(
    repository: &Repository,
    patch: &FilePatch,
    reverse: bool,
    current: &[u8],
) -> std::result::Result<Vec<u8>, Vec<String>> {
    let (old_path, new_path) = if reverse {
        (&patch.new_path, &patch.old_path)
    } else {
        (&patch.old_path, &patch.new_path)
    };
    let name = String::from_utf8_lossy(
        old_path
            .as_ref()
            .or(new_path.as_ref())
            .map_or(&b""[..], |n| n),
    )
    .into_owned();
    let does_not_apply =
        |first: String| vec![first, format!("error: {name}: patch does not apply")];
    if patch.binary_data {
        return Err(vec!["error: binary patches are not supported".to_string()]);
    }
    let format = repository.odb().format();
    let (pre, post) = if reverse {
        (&patch.new_oid, &patch.old_oid)
    } else {
        (&patch.old_oid, &patch.new_oid)
    };
    let full = |id: &Option<String>| {
        id.as_deref()
            .and_then(|hex| ObjectId::from_hex(format, BStr::new(hex)).ok())
    };
    let (Some(pre), Some(post)) = (full(pre), full(post)) else {
        return Err(does_not_apply(format!(
            "error: cannot apply binary patch to '{name}' without full index line"
        )));
    };
    if pre != ObjectId::zero(format) {
        let (actual, _) = rivet_core::object::hash_object(format, ObjectKind::Blob, current)
            .map_err(|e| vec![format!("error: {e}")])?;
        if actual != pre {
            return Err(does_not_apply(format!(
                "error: the patch applies to '{name}' ({actual}), which does not match the current contents."
            )));
        }
    }
    if post == ObjectId::zero(format) {
        return Ok(Vec::new());
    }
    match repository.odb().read_typed(&post, ObjectKind::Blob) {
        Ok(object) => Ok(object.into_data()),
        Err(_) => Err(vec![
            format!("error: missing binary patch data for '{name}'"),
            format!("error: binary patch does not apply to '{name}'"),
            format!("error: {name}: patch does not apply"),
        ]),
    }
}

type Pending = std::collections::BTreeMap<Vec<u8>, Option<(Vec<u8>, EntryMode)>>;

/// Write the new state of every touched path (and the `.rej` files) to the work tree and/or index.
fn write_pending(
    repository: &mut Repository,
    pending: &Pending,
    conflicts: &[(Vec<u8>, ThreeWayMerge)],
    rejects: &[(Vec<u8>, Vec<u8>)],
    options: ApplyOptions,
    update_index: bool,
) -> std::result::Result<(), Vec<String>> {
    use rivet_core::IndexExt as _;

    let fail = |e: &dyn std::fmt::Display| vec![format!("error: {e}")];
    for (path, state) in pending {
        let path_ref = BStr::new(&path[..]);
        match state {
            None => {
                if !options.cached {
                    let full = repository
                        .worktree()
                        .and_then(|w| w.join(path_ref))
                        .map_err(|e| fail(&e))?;
                    let _ = std::fs::remove_file(&full);
                    if let Some(root) = repository
                        .worktree()
                        .ok()
                        .and_then(|w| w.root().map(|r| r.to_path_buf()))
                    {
                        rivet_core::ops::prune_empty_dirs(&root, path_ref);
                    }
                }
                if update_index {
                    repository.index_mut().remove_path(path_ref);
                }
            }
            Some((data, mode)) => {
                let mut stat = None;
                if !options.cached {
                    let full = repository
                        .worktree()
                        .and_then(|w| w.join(path_ref))
                        .map_err(|e| fail(&e))?;
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| fail(&e))?;
                    }
                    std::fs::write(&full, data).map_err(|e| fail(&e))?;
                    use std::os::unix::fs::PermissionsExt as _;
                    let bits = if *mode == EntryMode::BlobExecutable {
                        0o755
                    } else {
                        0o644
                    };
                    std::fs::set_permissions(&full, std::fs::Permissions::from_mode(bits))
                        .map_err(|e| fail(&e))?;
                    stat = repository
                        .worktree()
                        .and_then(|w| w.stat(path_ref))
                        .map_err(|e| fail(&e))?;
                }
                // A conflicted path gets its stages below, not a stage-0 entry.
                if update_index && !conflicts.iter().any(|(conflicted, _)| conflicted == path) {
                    let oid = repository
                        .odb()
                        .write_raw(ObjectKind::Blob, data)
                        .map_err(|e| fail(&e))?;
                    repository.index_mut().remove_path(path_ref);
                    repository
                        .index_mut()
                        .insert(rivet_core::ops::make_index_entry(
                            path_ref,
                            *mode,
                            oid,
                            stat.as_ref(),
                        ));
                }
            }
        }
    }
    if update_index {
        for (path, done) in conflicts {
            let conflict = rivet_core::merge::Conflict {
                path: BString::from(path.clone()),
                kind: rivet_core::merge::ConflictKind::Content,
                base: done.base,
                ours: Some(done.ours),
                theirs: Some(done.theirs),
                hunks: Vec::new(),
                message: None,
            };
            rivet_core::merge::write_conflict_stages(repository.index_mut(), &[conflict])
                .map_err(|e| fail(&e))?;
        }
        repository.index_mut().sort();
        repository.write_index().map_err(|e| fail(&e))?;
    }
    for (path, text) in rejects {
        let full = repository
            .worktree()
            .and_then(|w| w.join(BStr::new(&path[..])))
            .map_err(|e| fail(&e))?;
        std::fs::write(&full, text).map_err(|e| fail(&e))?;
    }
    Ok(())
}
