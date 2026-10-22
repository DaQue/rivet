//! `notes`: attach free-form text to objects without changing them.
//!
//! The notes live in a commit history under `refs/notes/commits` (or `core.notesRef`,
//! `GIT_NOTES_REF`, `--ref`). Each commit's tree maps an annotated object's hex id to a blob holding
//! the note, split into `xx/yy/…` directories as the number of notes grows — the same layout and the
//! same fan-out heuristic as Git, so the tree ids match Git's byte for byte. `show`, `list`, `add`,
//! `append`, `remove` and `copy` are implemented; `edit`, `merge`, `prune`, `get-ref`, `-c`/`-C` and
//! `copy --stdin` are not. Rivet never starts an editor: a message must come from `-m` or `-F`.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};

use anyhow::{Result, bail};
use bstr::{BStr, BString};
use rivet_core::{ObjectId, ObjectKind, RefName, ReflogMessage, Repository};

use crate::cli::GlobalArgs;

/// Arguments to `rivet notes`. Without a subcommand the notes are listed.
#[derive(Debug, clap::Args)]
pub struct NotesArgs {
    /// Operate on this notes ref (`refs/notes/` is added when missing).
    #[arg(long = "ref", global = true, value_name = "REF")]
    pub notes_ref: Option<String>,

    /// The notes operation; `list` when omitted.
    #[command(subcommand)]
    pub command: Option<NotesCommand>,
}

/// The `notes` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum NotesCommand {
    /// List the notes, or the note blob of one object.
    List(ObjectArg),
    /// Show the note of an object.
    Show(ObjectArg),
    /// Add a note to an object.
    Add(AddArgs),
    /// Append to the note of an object.
    Append(AddArgs),
    /// Remove the notes of the given objects.
    Remove(RemoveArgs),
    /// Copy the note of one object to another.
    Copy(CopyArgs),
}

/// A single optional object.
#[derive(Debug, clap::Args)]
pub struct ObjectArg {
    /// The object; `HEAD` when omitted (`list` without it lists every note).
    #[arg(value_name = "OBJECT")]
    pub object: Option<String>,
}

/// Arguments of `notes add` and `notes append`.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// Use this text as (a paragraph of) the note. May be repeated.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Vec<String>,

    /// Take the note text from a file (`-` for standard input). Added after every `-m`.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    pub file: Vec<String>,

    /// Replace an existing note (`add` only).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Store an empty note instead of removing the note.
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// The object to annotate; `HEAD` when omitted.
    #[arg(value_name = "OBJECT")]
    pub object: Option<String>,
}

/// Arguments of `notes remove`.
#[derive(Debug, clap::Args)]
pub struct RemoveArgs {
    /// Do not fail for objects that have no note.
    #[arg(long = "ignore-missing")]
    pub ignore_missing: bool,

    /// The objects; `HEAD` when none is given.
    #[arg(value_name = "OBJECT")]
    pub objects: Vec<String>,
}

/// Arguments of `notes copy`.
#[derive(Debug, clap::Args)]
pub struct CopyArgs {
    /// Replace an existing note on the target.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// The object whose note is copied.
    #[arg(value_name = "FROM")]
    pub from: String,

    /// The object that receives the note; `HEAD` when omitted.
    #[arg(value_name = "TO")]
    pub to: Option<String>,
}

/// The notes of one notes ref, as they were when it was loaded.
struct Notes {
    /// The commit the ref pointed at, if it existed.
    head: Option<ObjectId>,
    /// Annotated object (lowercase hex) to the blob holding its note.
    map: BTreeMap<String, ObjectId>,
    /// Entries of the tree that are not notes, kept at their paths.
    others: Vec<(BString, rivet_core::object::EntryMode, ObjectId)>,
}

/// Handler for `rivet notes`.
///
/// # Exit codes
///
/// * `0` — done.
/// * `1` — the requested note does not exist, or an existing note would be overwritten.
/// * `128` — a fatal error (unresolvable object, no message given, unreadable `-F` file).
pub fn run_notes(global: &GlobalArgs, args: &NotesArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let name = notes_ref(&repository, args.notes_ref.as_deref())?;
    match &args.command {
        None => list(&repository, &name, None),
        Some(NotesCommand::List(a)) => list(&repository, &name, a.object.as_deref()),
        Some(NotesCommand::Show(a)) => show(&repository, &name, a.object.as_deref()),
        Some(NotesCommand::Add(a)) => put(&repository, &name, a, false),
        Some(NotesCommand::Append(a)) => put(&repository, &name, a, true),
        Some(NotesCommand::Remove(a)) => remove(&repository, &name, a),
        Some(NotesCommand::Copy(a)) => copy(&repository, &name, a),
    }
}

/// `--ref`, then `GIT_NOTES_REF`, then `core.notesRef`, then `refs/notes/commits`, expanded the
/// way Git expands a short notes ref name.
fn notes_ref(repository: &Repository, option: Option<&str>) -> Result<RefName> {
    let raw = match option {
        Some(r) => r.to_string(),
        None => match std::env::var("GIT_NOTES_REF") {
            Ok(r) if !r.is_empty() => r,
            _ => match repository.config_value(BStr::new("core.notesRef"))? {
                Some(r) => r.to_string(),
                None => "refs/notes/commits".to_string(),
            },
        },
    };
    let full = if raw.starts_with("refs/notes/") {
        raw
    } else if raw.starts_with("notes/") {
        format!("refs/{raw}")
    } else {
        format!("refs/notes/{raw}")
    };
    RefName::from_bytes(full.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid notes ref name '{full}'"))
}

fn resolve(repository: &Repository, spec: &str) -> Result<ObjectId> {
    rivet_core::revparse::resolve(
        repository.format(),
        repository.refs(),
        repository.odb(),
        BStr::new(spec.as_bytes()),
    )
    .map_err(|_| anyhow::anyhow!("failed to resolve '{spec}' as a valid ref."))
}

impl Notes {
    fn load(repository: &Repository, name: &RefName) -> Result<Self> {
        let mut notes = Notes {
            head: None,
            map: BTreeMap::new(),
            others: Vec::new(),
        };
        if repository.refs().try_read(name)?.is_none() {
            return Ok(notes);
        }
        let head = repository.refs().resolve_oid(name)?;
        notes.head = Some(head);
        let tree = repository.commit_tree(head)?;
        let hex_len = repository.format().len() * 2;
        for (path, (mode, oid)) in rivet_core::ops::flatten_tree(repository.odb(), tree)? {
            match note_key(&path, hex_len) {
                Some(key) => {
                    notes.map.insert(key, oid);
                }
                None => notes.others.push((path, mode, oid)),
            }
        }
        Ok(notes)
    }

    /// Write the tree for the current map and move the ref to a new commit on top of the old one.
    fn commit(&self, repository: &Repository, name: &RefName, message: &str) -> Result<()> {
        use rivet_core::IndexExt as _;
        let mut layout = Vec::new();
        let keys: Vec<(&str, ObjectId)> = self.map.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        if !keys.is_empty() {
            fan_out(&keys, 0, 0, &mut layout);
        }
        let mut index = rivet_core::Index::new();
        for (path, oid) in &layout {
            index.insert(rivet_core::ops::make_index_entry(
                BStr::new(path.as_bytes()),
                rivet_core::object::EntryMode::Blob,
                *oid,
                None,
            ));
        }
        for (path, mode, oid) in &self.others {
            index.insert(rivet_core::ops::make_index_entry(
                BStr::new(&path[..]),
                *mode,
                *oid,
                None,
            ));
        }
        index.sort();
        let tree = index.write_tree(repository.odb())?;
        let parents: Vec<ObjectId> = self.head.into_iter().collect();
        let commit = repository.write_commit_object(
            tree,
            &parents,
            BStr::new(format!("{message}\n").as_bytes()),
        )?;
        let who = repository.committer_signature()?;
        let reflog_text = format!("notes: {message}");
        let reflog = repository
            .should_log_ref(name)
            .then(|| ReflogMessage::new(&who, BStr::new(reflog_text.as_bytes())));
        // Compare-and-swap against the commit this write was based on, so a concurrent writer
        // makes this fail instead of silently dropping its note.
        repository.refs().update(name, commit, self.head, reflog)?;
        Ok(())
    }
}

/// The annotated object a notes-tree path stands for: every directory is two hex digits and the
/// whole path spells a full-length id.
fn note_key(path: &BString, hex_len: usize) -> Option<String> {
    let mut key = String::new();
    let parts: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
    for (i, part) in parts.iter().enumerate() {
        if !part.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        if i + 1 < parts.len() && part.len() != 2 {
            return None;
        }
        key.push_str(std::str::from_utf8(part).ok()?);
    }
    (key.len() == hex_len).then_some(key)
}

/// Lay the sorted `keys` (which share their first `depth` hex digits) out into tree paths, using
/// Git's fan-out rule: at every even depth within the current fan-out, if all sixteen children have
/// at least two notes below them there are plenty of notes and one more directory level is used for
/// everything beneath.
fn fan_out(
    keys: &[(&str, ObjectId)],
    depth: usize,
    fanout: usize,
    out: &mut Vec<(String, ObjectId)>,
) {
    if let [(key, oid)] = keys {
        let mut path = String::new();
        for level in 0..fanout {
            path.push_str(&key[2 * level..2 * level + 2]);
            path.push('/');
        }
        path.push_str(&key[2 * fanout..]);
        out.push((path, *oid));
        return;
    }
    let digit = |key: &str| key.as_bytes()[depth];
    let mut counts = [0usize; 16];
    for (key, _) in keys {
        counts[(digit(key) as char).to_digit(16).unwrap_or(0) as usize] += 1;
    }
    let fanout = if depth.is_multiple_of(2) && depth <= 2 * fanout && counts.iter().all(|c| *c >= 2)
    {
        fanout + 1
    } else {
        fanout
    };
    let mut start = 0;
    while start < keys.len() {
        let d = digit(keys[start].0);
        let end = start
            + keys[start..]
                .iter()
                .take_while(|(k, _)| digit(k) == d)
                .count();
        fan_out(&keys[start..end], depth + 1, fanout, out);
        start = end;
    }
}

fn list(repository: &Repository, name: &RefName, object: Option<&str>) -> Result<i32> {
    let notes = Notes::load(repository, name)?;
    let mut out = std::io::stdout().lock();
    match object {
        None => {
            for (key, blob) in &notes.map {
                writeln!(out, "{blob} {key}")?;
            }
        }
        Some(spec) => {
            let oid = resolve(repository, spec)?;
            match notes.map.get(&oid.to_string()) {
                Some(blob) => writeln!(out, "{blob}")?,
                None => {
                    eprintln!("error: no note found for object {oid}.");
                    return Ok(1);
                }
            }
        }
    }
    Ok(0)
}

fn show(repository: &Repository, name: &RefName, object: Option<&str>) -> Result<i32> {
    let notes = Notes::load(repository, name)?;
    let oid = resolve(repository, object.unwrap_or("HEAD"))?;
    let Some(blob) = notes.map.get(&oid.to_string()) else {
        eprintln!("error: no note found for object {oid}.");
        return Ok(1);
    };
    let data = repository.odb().read_typed(blob, ObjectKind::Blob)?;
    std::io::stdout().lock().write_all(data.data())?;
    Ok(0)
}

/// Git's `strbuf_stripspace`: trailing blanks off every line, runs of empty lines collapsed to
/// one, leading and trailing empty lines dropped, a final newline added to non-empty text.
fn stripspace(text: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut pending_blank = false;
    for line in text.split(|b| *b == b'\n') {
        let end = line
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(0, |i| i + 1);
        let line = &line[..end];
        if line.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push(b'\n');
            pending_blank = false;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// The note text the `-m` and `-F` options spell, or `None` when neither was given.
fn message_of(args: &AddArgs) -> Result<Option<Vec<u8>>> {
    if args.message.is_empty() && args.file.is_empty() {
        return Ok(None);
    }
    let mut buf: Vec<u8> = Vec::new();
    for text in &args.message {
        if !buf.is_empty() {
            buf.push(b'\n');
        }
        buf.extend_from_slice(text.as_bytes());
        buf = stripspace(&buf);
    }
    for file in &args.file {
        let mut data = Vec::new();
        if file == "-" {
            std::io::stdin().read_to_end(&mut data)?;
        } else {
            data = std::fs::read(file).map_err(|e| {
                anyhow::anyhow!(
                    "could not open or read '{file}': {}",
                    e.to_string().split(" (os error").next().unwrap_or_default()
                )
            })?;
        }
        if !buf.is_empty() {
            buf.push(b'\n');
        }
        buf.extend_from_slice(&data);
        buf = stripspace(&buf);
    }
    Ok(Some(buf))
}

/// `add` and `append`.
fn put(repository: &Repository, name: &RefName, args: &AddArgs, append: bool) -> Result<i32> {
    let verb = if append { "append" } else { "add" };
    let Some(mut text) = message_of(args)? else {
        bail!(
            "no note message given: pass -m or -F (rivet never starts an editor for 'notes {verb}')"
        );
    };
    let oid = resolve(repository, args.object.as_deref().unwrap_or("HEAD"))?;
    let key = oid.to_string();
    let mut notes = Notes::load(repository, name)?;
    let existing = notes.map.get(&key).copied();
    if append {
        if let Some(blob) = existing {
            let prev = repository.odb().read_typed(&blob, ObjectKind::Blob)?;
            let prev = prev.data();
            if !prev.is_empty() {
                let mut combined = prev.to_vec();
                if !text.is_empty() {
                    combined.push(b'\n');
                }
                combined.extend_from_slice(&text);
                text = combined;
            }
        }
    } else if existing.is_some() {
        if !args.force {
            eprintln!(
                "error: Cannot add notes. Found existing notes for object {oid}. Use '-f' to overwrite existing notes"
            );
            return Ok(1);
        }
        eprintln!("Overwriting existing notes for object {oid}");
    }
    if text.is_empty() && !args.allow_empty {
        eprintln!("Removing note for object {oid}");
        if notes.map.remove(&key).is_some() {
            notes.commit(
                repository,
                name,
                &format!("Notes removed by 'git notes {verb}'"),
            )?;
        }
        return Ok(0);
    }
    let blob = repository.odb().write_raw(ObjectKind::Blob, &text)?;
    notes.map.insert(key, blob);
    notes.commit(
        repository,
        name,
        &format!("Notes added by 'git notes {verb}'"),
    )?;
    Ok(0)
}

fn remove(repository: &Repository, name: &RefName, args: &RemoveArgs) -> Result<i32> {
    let mut notes = Notes::load(repository, name)?;
    let specs: Vec<&str> = if args.objects.is_empty() {
        vec!["HEAD"]
    } else {
        args.objects.iter().map(String::as_str).collect()
    };
    let mut status = 0;
    let mut removed = false;
    for spec in specs {
        let Ok(oid) = resolve(repository, spec) else {
            eprintln!("error: Failed to resolve '{spec}' as a valid ref.");
            status = 1;
            continue;
        };
        if notes.map.remove(&oid.to_string()).is_some() {
            eprintln!("Removing note for object {spec}");
            removed = true;
        } else {
            eprintln!("Object {spec} has no note");
            if !args.ignore_missing {
                status = 1;
            }
        }
    }
    if removed {
        notes.commit(repository, name, "Notes removed by 'git notes remove'")?;
    }
    Ok(status)
}

fn copy(repository: &Repository, name: &RefName, args: &CopyArgs) -> Result<i32> {
    let from = resolve(repository, &args.from)?;
    let to = resolve(repository, args.to.as_deref().unwrap_or("HEAD"))?;
    let mut notes = Notes::load(repository, name)?;
    let Some(blob) = notes.map.get(&from.to_string()).copied() else {
        eprintln!("error: missing notes on source object {from}. Cannot copy.");
        return Ok(1);
    };
    if notes.map.contains_key(&to.to_string()) {
        if !args.force {
            eprintln!(
                "error: Cannot copy notes. Found existing notes for object {to}. Use '-f' to overwrite existing notes"
            );
            return Ok(1);
        }
        eprintln!("Overwriting existing notes for object {to}");
    }
    notes.map.insert(to.to_string(), blob);
    notes.commit(repository, name, "Notes added by 'git notes copy'")?;
    Ok(0)
}
