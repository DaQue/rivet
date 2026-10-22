//! `blame`: attribute each line of a file to the commit that last changed it.
//!
//! The algorithm is Git's: start with every line blamed on the tip, and for each commit pass the
//! lines that a parent also has (by a line diff, using the same diff engine as `diff`) down to
//! that parent, trying parents in order; the lines no parent has stay with the commit. A file
//! that does not exist in a parent is looked for under its old name when the commit renamed it.
//!
//! Not implemented: `-C`/`-M` line movement detection, `--porcelain`, `--incremental`, `-w`,
//! `--since`, `-S`, regular-expression `-L` ranges, `--contents`; they are refused.

use anyhow::Result;
use bstr::BStr;
use rivet_core::diff::myers::{compact_edits, git_diff, split_lines};
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
}

/// Who a blamed line belongs to.
#[derive(Clone)]
struct Origin {
    commit: Option<ObjectId>,
    path: Vec<u8>,
    boundary: bool,
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

fn blob_at(
    repository: &Repository,
    tree: ObjectId,
    path: &[u8],
) -> Result<Option<(ObjectId, Vec<u8>)>> {
    let map = rivet_core::ops::flatten_tree(repository.odb(), tree)?;
    let Some((_, oid)) = map.get(BStr::new(path)) else {
        return Ok(None);
    };
    let data = repository
        .odb()
        .read_typed(oid, ObjectKind::Blob)?
        .into_data();
    Ok(Some((*oid, data)))
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
    let Some((_, committed)) = blob_at(&repository, tip_tree, &path)? else {
        let shown = String::from_utf8_lossy(&path);
        match &rev {
            Some(spec) => eprintln!("fatal: no such path {shown} in {spec}"),
            None => eprintln!("fatal: no such path '{shown}' in HEAD"),
        }
        return Ok(128);
    };

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

    let mut wanted = vec![false; total];
    if args.ranges.is_empty() {
        wanted.fill(true);
    }
    for range in &args.ranges {
        if range.starts_with('/') || range.contains(":/") {
            anyhow::bail!("blame: regular-expression -L ranges are not implemented");
        }
        match parse_range(range, total, &file) {
            Ok((start, end)) => {
                for flag in &mut wanted[start - 1..end] {
                    *flag = true;
                }
            }
            Err(error) => {
                eprintln!("fatal: {error}");
                return Ok(128);
            }
        }
    }

    // Pending work: (commit or the work tree, path, [(final line, line in that version)]).
    let mut origins: Vec<Option<Origin>> = vec![None; total];
    // (commit or work tree, path, contents there, [(final line, line there)])
    type Work = (Option<ObjectId>, Vec<u8>, Vec<u8>, Vec<(usize, usize)>);
    let mut queue: Vec<Work> = Vec::new();
    let initial: Vec<(usize, usize)> = (0..total).filter(|i| wanted[*i]).map(|i| (i, i)).collect();
    if top_is_worktree {
        queue.push((None, path.to_vec(), content.clone(), initial));
    } else {
        queue.push((Some(tip), path.to_vec(), committed.clone(), initial));
    }

    while let Some((commit, path, data, mut pending)) = queue.pop() {
        if pending.is_empty() {
            continue;
        }
        let parents = match commit {
            Some(oid) => repository.commit_parents(oid)?,
            None => vec![tip],
        };
        let child_tree = match commit {
            Some(oid) => repository.commit_tree(oid)?,
            None => tip_tree,
        };
        let child_lines = split_lines(&data);
        for parent in &parents {
            if pending.is_empty() {
                break;
            }
            let parent_tree = repository.commit_tree(*parent)?;
            let mut parent_path = path.clone();
            let mut found = blob_at(&repository, parent_tree, &parent_path)?;
            if found.is_none() && commit.is_some() {
                // Renamed in this commit? Look for the old name.
                let options = rivet_core::DiffOptions::new();
                let mut diff = rivet_core::tree_diff(
                    repository.format(),
                    parent_tree,
                    child_tree,
                    repository.odb(),
                    &options,
                )?;
                super::detect_renames(&repository, &mut diff, &options)?;
                for file in &diff.files {
                    if file.is_rename()
                        && file.new_path.as_deref().map(|p| &p[..]) == Some(&path[..])
                        && let Some(old) = &file.old_path
                    {
                        parent_path = old.to_vec();
                        found = blob_at(&repository, parent_tree, &parent_path)?;
                    }
                }
            }
            let Some((_, parent_data)) = found else {
                continue;
            };
            let parent_lines = split_lines(&parent_data);
            let edits = git_diff(&parent_lines, &child_lines);
            let edits = compact_edits(&parent_lines, &child_lines, &edits);
            let mut passed: Vec<(usize, usize)> = Vec::new();
            pending.retain(|&(final_line, local)| {
                for edit in edits.iter().filter(|e| !e.is_change()) {
                    if local >= edit.new_index && local < edit.new_index + edit.new_len {
                        passed.push((final_line, edit.old_index + (local - edit.new_index)));
                        return false;
                    }
                }
                true
            });
            if !passed.is_empty() {
                queue.push((Some(*parent), parent_path, parent_data, passed));
            }
        }
        let boundary = parents.is_empty();
        for (final_line, _) in pending {
            origins[final_line] = Some(Origin {
                commit,
                path: path.clone(),
                boundary,
            });
        }
    }

    // Render.
    let shown: Vec<usize> = (0..total).filter(|i| wanted[*i]).collect();
    let any_boundary = shown
        .iter()
        .any(|&i| origins[i].as_ref().expect("blamed").boundary);
    let abbrev = if args.long {
        repository.format().hex_len()
    } else if any_boundary {
        8
    } else {
        7
    };
    let mut people: std::collections::HashMap<Option<ObjectId>, (String, String, i64, i32)> =
        std::collections::HashMap::new();
    for &i in &shown {
        let origin = origins[i].as_ref().expect("every line is blamed");
        if people.contains_key(&origin.commit) {
            continue;
        }
        let entry = match origin.commit {
            Some(oid) => {
                let object = repository.odb().read_typed(&oid, ObjectKind::Commit)?;
                let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
                (
                    String::from_utf8_lossy(&commit.author.name).into_owned(),
                    format!("<{}>", String::from_utf8_lossy(&commit.author.email)),
                    commit.author.when,
                    commit.author.tz_offset_minutes,
                )
            }
            None => {
                let who = repository.author_signature()?;
                (
                    "Not Committed Yet".to_string(),
                    "<not.committed.yet>".to_string(),
                    who.when,
                    who.tz_offset_minutes,
                )
            }
        };
        people.insert(origin.commit, entry);
    }
    let name_of = |commit: &Option<ObjectId>| -> String {
        let person = &people[commit];
        if args.email {
            person.1.clone()
        } else {
            person.0.clone()
        }
    };
    let name_width = shown
        .iter()
        .map(|&i| {
            name_of(&origins[i].as_ref().expect("blamed").commit)
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0);
    let number_width = shown.last().map(|n| (n + 1).to_string().len()).unwrap_or(1);
    let show_file = args.show_name
        || shown
            .iter()
            .any(|&i| origins[i].as_ref().expect("blamed").path != path[..]);
    let file_width = shown
        .iter()
        .map(|&i| origins[i].as_ref().expect("blamed").path.len())
        .max()
        .unwrap_or(0);

    let mut out: Vec<u8> = Vec::new();
    for &i in &shown {
        let origin = origins[i].as_ref().expect("blamed");
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
            let date = rivet_core::ops::format_date_style("iso", person.2, person.3, person.2)
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
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&out).and_then(|_| stdout.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    Ok(0)
}
