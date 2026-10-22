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
//! Binary patches, `--3way`, whitespace fixing and fuzz are not implemented and are refused.

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
                } else if l.starts_with(b"similarity index ")
                    || l.starts_with(b"dissimilarity index ")
                    || l.starts_with(b"index ")
                {
                } else if l.starts_with(b"GIT binary patch") || l.starts_with(b"Binary files ") {
                    bail!("binary patches are not supported");
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
            patch.hunks.push(hunk);
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
}

/// Apply `patch`'s hunks to `data`; `reverse` swaps the two sides.
///
/// # Errors
///
/// The line number of the first hunk that does not fit.
pub fn apply_hunks(
    data: &[u8],
    hunks: &[Hunk],
    reverse: bool,
    unidiff_zero: bool,
) -> std::result::Result<Vec<u8>, HunkFailure> {
    let mut content = split_content(data);
    let mut drift: isize = 0;
    for hunk in hunks {
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
            return Err(HunkFailure {
                line: hunk.old_start.max(1),
            });
        };
        drift = at as isize - expected;
        let ends_here = at + pre.len() == content.lines.len();
        content.lines.splice(at..at + pre.len(), post);
        if ends_here {
            content.trailing_newline = !new_no_newline;
        }
    }
    Ok(join_content(&content))
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
}

use bstr::BStr;
use rivet_core::object::EntryMode;
use rivet_core::{ObjectKind, Repository};

/// Apply `patches` to `repository`. On failure nothing has been written and the returned lines
/// are the diagnostics Git prints (`error: …`).
pub fn apply_to_repo(
    repository: &mut Repository,
    patches: &[FilePatch],
    options: ApplyOptions,
) -> std::result::Result<Vec<String>, Vec<String>> {
    use rivet_core::IndexExt as _;
    use std::collections::BTreeMap;
    // Pending state per path: `Some((data, mode))` once written, `None` once removed.
    let mut pending: BTreeMap<Vec<u8>, Option<(Vec<u8>, EntryMode)>> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let show = |p: &[u8]| String::from_utf8_lossy(p).into_owned();

    for patch in patches {
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
        if is_new && read(repository, &pending, &target).is_some() {
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
        let (base_data, base_mode) = match &source {
            Some((_, (data, mode))) => (data.clone(), *mode),
            None => (Vec::new(), EntryMode::Blob),
        };
        let result = match apply_hunks(
            &base_data,
            &patch.hunks,
            options.reverse,
            options.unidiff_zero,
        ) {
            Ok(result) => result,
            Err(failure) => {
                errors.push(format!(
                    "error: patch failed: {}:{}",
                    show(source.as_ref().map(|s| s.0.as_slice()).unwrap_or(&target)),
                    failure.line
                ));
                errors.push(format!(
                    "error: {}: patch does not apply",
                    show(source.as_ref().map(|s| s.0.as_slice()).unwrap_or(&target))
                ));
                continue;
            }
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
        pending.insert(target, Some((result, mode)));
    }
    if errors.iter().any(|line| !line.starts_with("warning:")) {
        return Err(errors);
    }
    if options.check {
        return Ok(warnings);
    }

    let fail = |e: &dyn std::fmt::Display| vec![format!("error: {e}")];
    for (path, state) in &pending {
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
                if options.index || options.cached {
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
                if options.index || options.cached {
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
    if options.index || options.cached {
        repository.index_mut().sort();
        // The cached tree no longer describes the entries.
        repository
            .index_mut()
            .extensions_mut()
            .retain(|e| !e.is_tree());
        repository.write_index().map_err(|e| fail(&e))?;
    }
    Ok(warnings)
}
