//! `mktree`: build tree objects from `ls-tree` formatted text on standard input.

use std::io::{Read, Write};

use anyhow::{Result, bail};
use bstr::{BStr, ByteSlice};
use rivet_core::{ObjectId, ObjectKind, Repository};

use crate::cli::GlobalArgs;

/// Arguments of `mktree`: everything after the command, in Git's own grammar.
#[derive(Debug, clap::Args)]
pub struct MkTreeArgs {
    /// `[-z] [--[no-]missing] [--[no-]batch]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = "usage: git mktree [-z] [--missing] [--batch]

    -z                    input is NUL terminated
    --[no-]missing        allow missing objects
    --[no-]batch          allow creation of more than one tree

";

/// One parsed input line.
struct Entry {
    mode: u32,
    name: Vec<u8>,
    oid: ObjectId,
}

/// `rivet mktree [-z] [--[no-]missing] [--[no-]batch] < <ls-tree text>`.
///
/// # Caveats
///
/// * **Entries are sorted, not deduplicated.** Like Git, two lines with the same name both end up
///   in the tree, in input order. Modes are written as the octal number given, not canonicalised.
/// * **A mode with a sign or one that does not fit 32 bits is refused by name**, because Git's
///   `strtoul` reading of such a mode wraps in ways that are not reproduced here.
/// * **Gitlinks are never looked up**; every other entry must exist unless `--missing` is given.
pub fn run_mktree(global: &GlobalArgs, args: &MkTreeArgs) -> Result<i32> {
    let mut nul = false;
    let mut missing = false;
    let mut batch = false;
    let mut only_positional = false;
    for word in &args.args {
        if only_positional || word == "-" || !word.starts_with('-') {
            continue;
        }
        match word.as_str() {
            "--" => only_positional = true,
            "-z" => nul = true,
            "-h" => {
                print!("{USAGE}");
                return Ok(0);
            }
            long if long.starts_with("--") => {
                let name = &long[2..];
                let options = ["missing", "no-missing", "batch", "no-batch"];
                let found: Vec<&str> = options
                    .iter()
                    .copied()
                    .filter(|option| !name.is_empty() && option.starts_with(name))
                    .collect();
                let exact = options.iter().copied().find(|option| *option == name);
                match exact.or_else(|| (found.len() == 1).then(|| found[0])) {
                    Some("missing") => missing = true,
                    Some("no-missing") => missing = false,
                    Some("batch") => batch = true,
                    Some("no-batch") => batch = false,
                    _ => {
                        if found.len() > 1 {
                            eprintln!(
                                "error: ambiguous option: {name} (could be --{} or --{})",
                                found[0], found[1]
                            );
                        } else {
                            eprintln!("error: unknown option `{name}'");
                        }
                        eprint!("{USAGE}");
                        return Ok(129);
                    }
                }
            }
            short => {
                // Clustered short options: only `z` exists.
                for letter in short[1..].chars() {
                    if letter == 'z' {
                        nul = true;
                    } else {
                        eprintln!("error: unknown switch `{letter}'");
                        eprint!("{USAGE}");
                        return Ok(129);
                    }
                }
            }
        }
    }

    let repository = super::open(global)?;
    let mut input = Vec::new();
    std::io::stdin().lock().read_to_end(&mut input)?;
    let terminator = if nul { b'\0' } else { b'\n' };
    let mut records: Vec<&[u8]> = input.split(|b| *b == terminator).collect();
    if records.last().is_some_and(|last| last.is_empty()) {
        records.pop();
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut pending = false;
    for record in records {
        if record.is_empty() {
            if !batch {
                bail!("input format error: (blank line only valid in batch mode)");
            }
            write_tree(&repository, &mut entries)?;
            pending = false;
            continue;
        }
        pending = true;
        entries.push(parse_line(&repository, record, nul, missing)?);
    }
    if !batch || pending {
        write_tree(&repository, &mut entries)?;
    }
    Ok(0)
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// The kind of object a mode stands for: directory, gitlink or anything else (a blob).
fn mode_kind(mode: u32) -> ObjectKind {
    match mode & 0o170000 {
        0o040000 => ObjectKind::Tree,
        0o160000 => ObjectKind::Commit,
        _ => ObjectKind::Blob,
    }
}

/// `<mode> SP <type> SP <hex-id> TAB <name>`, checked the way `git mktree` checks it.
fn parse_line(repository: &Repository, line: &[u8], nul: bool, missing: bool) -> Result<Entry> {
    let shown = || String::from_utf8_lossy(line).into_owned();
    let bad = || anyhow::anyhow!("input format error: {}", shown());

    let text = line.trim_start_with(|c| c.is_ascii_whitespace());
    let text = text.strip_prefix(b"+").unwrap_or(text);
    let digits = text
        .iter()
        .take_while(|b| (b'0'..=b'7').contains(b))
        .count();
    if digits == 0 || text.get(digits) != Some(&b' ') {
        if text.first() == Some(&b'-') && text[1..].first().is_some_and(u8::is_ascii_digit) {
            bail!("mktree: a negative mode is not implemented");
        }
        return Err(bad());
    }
    let mode = u32::from_str_radix(std::str::from_utf8(&text[..digits])?, 8)
        .map_err(|_| anyhow::anyhow!("mktree: a mode wider than 32 bits is not implemented"))?;
    let rest = &text[digits + 1..];
    let Some(space) = rest.find_byte(b' ') else {
        return Err(bad());
    };
    let type_name = &rest[..space];
    let rest = &rest[space + 1..];
    let hex_len = repository.format().hex_len();
    if type_name.is_empty() || rest.len() < hex_len + 1 || rest[hex_len] != b'\t' {
        return Err(bad());
    }
    let hex = rest[..hex_len].to_ascii_lowercase();
    let Ok(oid) = ObjectId::from_hex(repository.format(), BStr::new(&hex)) else {
        return Err(bad());
    };
    let name_text = &rest[hex_len + 1..];

    let declared = match type_name {
        b"blob" => ObjectKind::Blob,
        b"tree" => ObjectKind::Tree,
        b"commit" => ObjectKind::Commit,
        b"tag" => ObjectKind::Tag,
        other => bail!("invalid object type \"{}\"", String::from_utf8_lossy(other)),
    };
    let name: Vec<u8> = if !nul && name_text.first() == Some(&b'"') {
        unquote(name_text).ok_or_else(|| anyhow::anyhow!("invalid quoting"))?
    } else {
        name_text.to_vec()
    };
    if name.contains(&b'/') {
        bail!("path {} contains slash", String::from_utf8_lossy(&name));
    }

    let expected = mode_kind(mode);
    let shown_name = String::from_utf8_lossy(&name).into_owned();
    if declared != expected {
        bail!(
            "entry '{shown_name}' object type ({}) doesn't match mode type ({})",
            kind_name(declared),
            kind_name(expected)
        );
    }
    if !missing && expected != ObjectKind::Commit {
        match repository.odb().read_header(&oid) {
            Err(_) => bail!("entry '{shown_name}' object {oid} is unavailable"),
            Ok((found, _)) if found != declared => bail!(
                "entry '{shown_name}' object {oid} is a {} but specified type was ({})",
                kind_name(found),
                kind_name(declared)
            ),
            Ok(_) => {}
        }
    }
    Ok(Entry { mode, name, oid })
}

/// The bytes inside a leading C-style quoted string; anything after the closing quote is ignored.
fn unquote(text: &[u8]) -> Option<Vec<u8>> {
    let inner = text.strip_prefix(b"\"")?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        match inner[i] {
            b'"' => return Some(out),
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

/// Sort the pending entries by Git's tree order (stable, so equal names keep input order), write
/// the tree and print its id. The entry list is left empty.
fn write_tree(repository: &Repository, entries: &mut Vec<Entry>) -> Result<()> {
    let key = |entry: &Entry| {
        let mut key = entry.name.clone();
        if mode_kind(entry.mode) == ObjectKind::Tree {
            key.push(b'/');
        }
        key
    };
    entries.sort_by_cached_key(key);
    let mut payload = Vec::new();
    for entry in entries.iter() {
        payload.extend_from_slice(format!("{:o} ", entry.mode).as_bytes());
        payload.extend_from_slice(&entry.name);
        payload.push(0);
        payload.extend_from_slice(entry.oid.as_bytes());
    }
    entries.clear();
    let oid = repository.odb().write_raw(ObjectKind::Tree, &payload)?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{oid}")?;
    out.flush()?;
    Ok(())
}
