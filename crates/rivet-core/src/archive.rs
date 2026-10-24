//! `git archive`: write a tree as a tar, gzip-compressed tar or zip stream.
//!
//! The bytes follow Git's `archive-tar.c` and `archive-zip.c`: a pax global header carrying the
//! commit id, entries in tree order with directories before their contents, mode bits as
//! `tar.umask = 002` leaves them, `root`/`root` owners, every entry stamped with the commit time,
//! and the stream padded to whole 10240-byte records with at least two zero blocks at the end.
//! Zip entries are stored unless deflating makes them smaller, the commit id is the archive
//! comment, and every entry carries an extended-timestamp field.
//!
//! # Caveats
//!
//! * **Compressed output is not byte-identical to Git's.** Git pipes a tar through `gzip -cn`,
//!   whose deflate encoder is not zlib's, and deflates zip entries with zlib. The decompressed
//!   tar and the extracted zip entries are identical; the compressed bytes may differ.
//! * `export-ignore` is honoured, read from the `.gitattributes` files of the archived tree (of the
//!   work tree with [`ArchiveOptions::worktree_attributes`]) and from `info/attributes`. `export-subst` is refused rather than ignored, so a file that wants
//!   `$Format:...$` expansion is never written out unexpanded.
//! * Pathspecs are literal paths and `*`/`?`/`[...]` globs; pathspec magic (`:(exclude)`, …) is
//!   refused.
//! * Zip64 is not written: an archive of 65 535 entries or more, or with a member of 4 GiB or
//!   more, is refused.

use std::io::Write;

use bstr::{BStr, BString, ByteSlice};

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::ignore::{IgnoreSource, PathKind, Pattern};
use crate::object::{AnyObject, EntryMode, ObjectKind, Tree};
use crate::repo::Repository;

/// The archive formats Git ships with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// An uncompressed POSIX tar stream.
    Tar,
    /// A tar stream run through gzip (`tar.gz` and `tgz`).
    TarGz,
    /// A zip file.
    Zip,
}

/// A file that is not in the tree but goes into the archive (`--add-file`).
#[derive(Clone, Debug)]
pub struct ArchiveFile {
    /// Its path in the archive, prefix included.
    pub path: BString,
    /// `0o100644` or `0o100755`.
    pub mode: u32,
    /// Its contents.
    pub data: Vec<u8>,
}

/// What to write and how.
#[derive(Clone, Debug)]
pub struct ArchiveOptions {
    /// The container format.
    pub format: ArchiveFormat,
    /// Prepended to every path. When it ends in `/` the archive starts with that directory.
    pub prefix: BString,
    /// The compression level (`-0` … `-9`); `None` is the default (6).
    pub level: Option<u32>,
    /// The time stamped on every entry; `None` takes the commit's time, or now for a bare tree.
    pub time: Option<i64>,
    /// Limit the archive to these paths; empty means everything.
    pub paths: Vec<BString>,
    /// Files to add after the tree's own entries.
    pub extra_files: Vec<ArchiveFile>,
    /// Name every entry on standard error as it is written.
    pub verbose: bool,
    /// Archive only this directory of the tree (`/`-separated, no leading or trailing slash), as
    /// `git archive` does when run from a subdirectory; empty for the whole tree.
    pub base: BString,
    /// Read `.gitattributes` from the work tree instead of from the archived tree (a directory
    /// without one has no attributes): `--worktree-attributes`. Ignored for a bare repository.
    pub worktree_attributes: bool,
}

impl ArchiveOptions {
    /// Options for `format` with no prefix, no path limit and no extra files.
    pub fn new(format: ArchiveFormat) -> Self {
        Self {
            format,
            prefix: BString::default(),
            level: None,
            time: None,
            paths: Vec::new(),
            extra_files: Vec::new(),
            verbose: false,
            base: BString::default(),
            worktree_attributes: false,
        }
    }
}

/// Where an entry's bytes come from.
enum Source {
    None,
    Blob(ObjectId),
    Bytes(Vec<u8>),
}

struct Item {
    path: Vec<u8>,
    mode: u32,
    oid: Option<ObjectId>,
    source: Source,
}

fn fatal(message: impl Into<String>) -> Error {
    Error::Message(message.into())
}

/// Write `treeish` (a commit, a tag, or a tree) as an archive to `out`.
///
/// # Caveats
///
/// * Every pathspec must match something, and that is checked before the first byte is written:
///   a failed `archive` leaves `out` untouched.
/// * The commit id goes into the stream only when `treeish` peels to a commit.
///
/// # Errors
///
/// [`Error::Message`] for an unmatched pathspec, an unsupported feature (see the module caveats)
/// or an object that is not a commit, tag or tree; read and write failures otherwise.
pub fn archive(
    repo: &Repository,
    treeish: ObjectId,
    options: &ArchiveOptions,
    out: &mut dyn Write,
) -> Result<()> {
    let (mut tree, commit, commit_time) = peel(repo, treeish)?;
    for part in options.base.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
        let object = repo.odb().read_typed(&tree, ObjectKind::Tree)?;
        let parsed = Tree::parse(repo.format(), object.data())?;
        tree = match parsed
            .entries
            .iter()
            .find(|e| e.name == part && e.mode == EntryMode::Tree)
        {
            Some(entry) => entry.oid,
            None => return Err(fatal("current working directory is untracked")),
        };
    }
    let time = options.time.or(commit_time).unwrap_or_else(now);
    let specs = parse_specs(&options.paths)?;

    let mut attrs = Attributes::new(repo, options)?;
    let mut items: Vec<Item> = Vec::new();
    let prefix: &[u8] = options.prefix.as_ref();
    if prefix.ends_with(b"/") {
        items.push(Item {
            path: prefix.to_vec(),
            mode: 0o040000,
            oid: None,
            source: Source::None,
        });
    }
    let mut matched = vec![false; specs.len()];
    walk(
        repo,
        tree,
        &mut Vec::new(),
        prefix,
        &specs,
        &mut matched,
        false,
        &mut attrs,
        &mut items,
    )?;
    for (spec, hit) in specs.iter().zip(&matched) {
        if !hit && !spec.exclude {
            return Err(fatal(format!(
                "pathspec '{}' did not match any files",
                spec.text.as_bstr()
            )));
        }
    }
    for file in &options.extra_files {
        items.push(Item {
            path: file.path.to_vec(),
            mode: file.mode,
            oid: None,
            source: Source::Bytes(file.data.clone()),
        });
    }

    if options.verbose {
        for item in &items {
            eprintln!("{}", item.path.as_bstr());
        }
    }
    match options.format {
        ArchiveFormat::Tar => write_tar(repo, &items, commit, time, out),
        ArchiveFormat::TarGz => {
            let level = options.level.unwrap_or(6).min(9);
            let mut encoder = GzipWriter::new(out, level);
            write_tar(repo, &items, commit, time, &mut encoder)?;
            encoder.finish()
        }
        ArchiveFormat::Zip => write_zip(repo, &items, commit, time, options.level, out),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Peel tags until a commit or tree: the tree to archive, the commit id and the commit time.
fn peel(repo: &Repository, mut oid: ObjectId) -> Result<(ObjectId, Option<ObjectId>, Option<i64>)> {
    loop {
        let object = repo.odb().read(&oid)?;
        match object.parse()? {
            AnyObject::Tag(tag) => oid = tag.object,
            AnyObject::Commit(commit) => {
                return Ok((commit.tree, Some(oid), Some(commit.committer.when)));
            }
            AnyObject::Tree(_) => return Ok((oid, None, None)),
            _ => return Err(fatal("not a tree object")),
        }
    }
}

// --- pathspecs -----------------------------------------------------------------------------

struct Spec {
    text: BString,
    glob: bool,
    /// `:(exclude)`, `:!` or `:^`: removes what it matches from the result.
    exclude: bool,
}

fn parse_specs(paths: &[BString]) -> Result<Vec<Spec>> {
    let mut out = Vec::new();
    for path in paths {
        let mut text: &[u8] = path.as_ref();
        let mut exclude = false;
        if let Some(rest) = text.strip_prefix(b":(exclude)") {
            exclude = true;
            text = rest;
        } else if let Some(rest) = text
            .strip_prefix(b":!")
            .or_else(|| text.strip_prefix(b":^"))
        {
            exclude = true;
            text = rest;
        } else if text.starts_with(b":") {
            return Err(fatal(format!(
                "pathspec magic is not implemented: '{}'",
                path.as_bstr()
            )));
        }
        while let Some(rest) = text.strip_prefix(b"./") {
            text = rest;
        }
        while text.len() > 1 && text.ends_with(b"/") {
            text = &text[..text.len() - 1];
        }
        let text = if text == b"." { b"" } else { text };
        out.push(Spec {
            text: BString::from(text),
            glob: text.iter().any(|b| matches!(b, b'*' | b'?' | b'[')),
            exclude,
        });
    }
    Ok(out)
}

/// How a tree entry relates to the pathspecs.
#[derive(PartialEq, Eq)]
enum Interest {
    /// Not wanted, and nothing below it is either.
    No,
    /// A directory on the way to a wanted path: write it, then look inside.
    Descend,
    /// Wanted, and so is everything below it.
    All,
}

fn interest(
    specs: &[Spec],
    matched: &mut [bool],
    path: &[u8],
    is_dir: bool,
    inside: bool,
) -> Interest {
    // An exclusion wins over every inclusion, and a lone exclusion means "everything else".
    for spec in specs.iter().filter(|s| s.exclude) {
        let text: &[u8] = spec.text.as_ref();
        let hit = if spec.glob {
            glob(text, path)
        } else {
            path == text || (path.starts_with(text) && path.get(text.len()) == Some(&b'/'))
        };
        if hit {
            return Interest::No;
        }
    }
    if inside || specs.iter().all(|s| s.exclude) {
        return Interest::All;
    }
    let mut result = Interest::No;
    for (i, spec) in specs.iter().enumerate().filter(|(_, s)| !s.exclude) {
        let text: &[u8] = spec.text.as_ref();
        if text.is_empty() {
            matched[i] = true;
            return Interest::All;
        }
        if spec.glob {
            if glob(text, path) {
                matched[i] = true;
                return Interest::All;
            }
            // The literal directory part of the pattern decides whether to look inside.
            let literal_end = text
                .iter()
                .position(|b| matches!(b, b'*' | b'?' | b'['))
                .unwrap_or(text.len());
            let literal = match text[..literal_end].iter().rposition(|b| *b == b'/') {
                Some(i) => &text[..=i],
                None => &text[..0],
            };
            if is_dir {
                let mut dir = path.to_vec();
                dir.push(b'/');
                if dir.starts_with(literal) || literal.starts_with(&dir) {
                    result = Interest::Descend;
                }
            }
        } else if path == text || (path.starts_with(text) && path.get(text.len()) == Some(&b'/')) {
            matched[i] = true;
            return Interest::All;
        } else if is_dir && text.starts_with(path) && text.get(path.len()) == Some(&b'/') {
            result = Interest::Descend;
        }
    }
    result
}

/// Shell-style matching where `*` and `?` also match `/`, as Git's pathspecs do.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob(rest, &text[i..])),
        Some((b'?', rest)) => !text.is_empty() && glob(rest, &text[1..]),
        Some((b'[', rest)) => {
            let Some(close) = rest.iter().skip(1).position(|b| *b == b']').map(|i| i + 1) else {
                return text.first() == Some(&b'[') && glob(rest, &text[1..]);
            };
            let Some((&c, tail)) = text.split_first() else {
                return false;
            };
            let mut set = &rest[..close];
            let negate = matches!(set.first(), Some(b'!' | b'^'));
            if negate {
                set = &set[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    hit |= set[i] <= c && c <= set[i + 2];
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            hit != negate && glob(&rest[close + 1..], tail)
        }
        Some((&c, rest)) => text.first() == Some(&c) && glob(rest, &text[1..]),
    }
}

// --- attributes ----------------------------------------------------------------------------

/// The value of an attribute line for one pattern.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    Set,
    Unset,
    Unspecified,
    Value,
}

struct Rule {
    pattern: Pattern,
    export_ignore: Option<Setting>,
    export_subst: Option<Setting>,
}

struct Attributes {
    /// Lowest priority first: the tree's files from the root down, then `info/attributes`.
    frames: Vec<Vec<Rule>>,
    info: Vec<Rule>,
    /// The work tree directory whose `.gitattributes` files take the place of the tree's.
    worktree: Option<std::path::PathBuf>,
}

fn parse_attribute_file(data: &[u8], base: &[u8]) -> Vec<Rule> {
    let mut rules = Vec::new();
    for line in data.split(|b| *b == b'\n') {
        let line = line.trim_with(|c| c.is_ascii_whitespace());
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let split = line
            .iter()
            .position(|b| b.is_ascii_whitespace())
            .unwrap_or(line.len());
        let (pattern, rest) = line.split_at(split);
        let mut export_ignore = None;
        let mut export_subst = None;
        for word in rest.fields() {
            let (name, setting) = if let Some(n) = word.strip_prefix(b"-") {
                (n, Setting::Unset)
            } else if let Some(n) = word.strip_prefix(b"!") {
                (n, Setting::Unspecified)
            } else if let Some(i) = word.iter().position(|b| *b == b'=') {
                (&word[..i], Setting::Value)
            } else {
                (word, Setting::Set)
            };
            match name {
                b"export-ignore" => export_ignore = Some(setting),
                b"export-subst" => export_subst = Some(setting),
                _ => {}
            }
        }
        if export_ignore.is_none() && export_subst.is_none() {
            continue;
        }
        // Attribute patterns have no negation; a trailing slash matches the directory itself (and
        // so, for an archive, everything below it) but nothing inside it on its own.
        if pattern.starts_with(b"!") || pattern.starts_with(b"[attr]") {
            continue;
        }
        if let Ok(Some(pattern)) = Pattern::parse(
            BStr::new(pattern),
            IgnoreSource::CommandLine,
            0,
            BStr::new(base),
        ) {
            rules.push(Rule {
                pattern,
                export_ignore,
                export_subst,
            });
        }
    }
    rules
}

impl Attributes {
    fn new(repo: &Repository, options: &ArchiveOptions) -> Result<Self> {
        let worktree = options
            .worktree_attributes
            .then(|| repo.worktree_path())
            .flatten()
            .map(|root| {
                let mut dir = root.to_path_buf();
                for part in options.base.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
                    dir.push(part.to_str_lossy().as_ref());
                }
                dir
            });
        let path = repo.common_dir().join("info/attributes");
        let info = match std::fs::read(&path) {
            Ok(data) => parse_attribute_file(&data, b""),
            Err(_) => Vec::new(),
        };
        Ok(Self {
            frames: Vec::new(),
            info,
            worktree,
        })
    }

    fn lookup(&self, path: &[u8], is_dir: bool, pick: fn(&Rule) -> Option<Setting>) -> Setting {
        let kind = if is_dir {
            PathKind::Directory
        } else {
            PathKind::File
        };
        let mut result = Setting::Unspecified;
        for frame in self.frames.iter().chain(std::iter::once(&self.info)) {
            for rule in frame {
                if let Some(setting) = pick(rule)
                    && rule.pattern.matches(BStr::new(path), kind)
                {
                    result = setting;
                }
            }
        }
        result
    }

    fn export_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        self.lookup(path, is_dir, |r| r.export_ignore) == Setting::Set
    }

    fn export_subst(&self, path: &[u8]) -> bool {
        self.lookup(path, false, |r| r.export_subst) == Setting::Set
    }
}

// --- the walk ------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn walk(
    repo: &Repository,
    tree: ObjectId,
    dir: &mut Vec<u8>,
    prefix: &[u8],
    specs: &[Spec],
    matched: &mut [bool],
    inside: bool,
    attrs: &mut Attributes,
    items: &mut Vec<Item>,
) -> Result<()> {
    let object = repo.odb().read_typed(&tree, ObjectKind::Tree)?;
    let parsed = Tree::parse(repo.format(), object.data())?;
    // This directory's `.gitattributes` applies to all of it, whatever the entry order.
    let from_worktree = attrs.worktree.as_ref().and_then(|root| {
        let mut path = root.clone();
        for part in dir.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
            path.push(part.to_str_lossy().as_ref());
        }
        std::fs::read(path.join(".gitattributes")).ok()
    });
    let in_tree = parsed
        .entries
        .iter()
        .find(|e| e.name == ".gitattributes" && !matches!(e.mode, EntryMode::Tree));
    let pushed = match (from_worktree, in_tree) {
        (Some(data), _) => {
            let base = dir.strip_suffix(b"/").unwrap_or(dir);
            attrs.frames.push(parse_attribute_file(&data, base));
            true
        }
        // With work tree attributes the tree's own files are not consulted at all.
        (None, Some(_)) if attrs.worktree.is_some() => false,
        (None, Some(entry)) => {
            let blob = repo.odb().read_typed(&entry.oid, ObjectKind::Blob)?;
            let base = dir.strip_suffix(b"/").unwrap_or(dir);
            attrs.frames.push(parse_attribute_file(blob.data(), base));
            true
        }
        (None, None) => false,
    };
    for entry in &parsed.entries {
        let base_len = dir.len();
        dir.extend_from_slice(&entry.name);
        let is_dir = matches!(entry.mode, EntryMode::Tree | EntryMode::Commit);
        // A gitlink is written like a directory but has nothing inside to look at.
        let want = interest(
            specs,
            matched,
            dir,
            matches!(entry.mode, EntryMode::Tree),
            inside,
        );
        if want != Interest::No && !attrs.export_ignored(dir, is_dir) {
            let mut path = prefix.to_vec();
            path.extend_from_slice(dir);
            match entry.mode {
                EntryMode::Tree => {
                    path.push(b'/');
                    items.push(Item {
                        path,
                        mode: 0o040000,
                        oid: None,
                        source: Source::None,
                    });
                    dir.push(b'/');
                    let inside = inside || want == Interest::All;
                    walk(
                        repo, entry.oid, dir, prefix, specs, matched, inside, attrs, items,
                    )?;
                }
                EntryMode::Commit => {
                    path.push(b'/');
                    items.push(Item {
                        path,
                        mode: 0o040000,
                        oid: None,
                        source: Source::None,
                    });
                }
                mode => {
                    if attrs.export_subst(dir) {
                        return Err(fatal(format!(
                            "export-subst is not implemented ({})",
                            dir.as_bstr()
                        )));
                    }
                    items.push(Item {
                        path,
                        mode: mode.to_u32(),
                        oid: Some(entry.oid),
                        source: Source::Blob(entry.oid),
                    });
                }
            }
        }
        dir.truncate(base_len);
    }
    if pushed {
        attrs.frames.pop();
    }
    Ok(())
}

fn contents(repo: &Repository, source: &Source) -> Result<Vec<u8>> {
    match source {
        Source::None => Ok(Vec::new()),
        Source::Bytes(bytes) => Ok(bytes.clone()),
        Source::Blob(oid) => Ok(repo
            .odb()
            .read_typed(oid, ObjectKind::Blob)?
            .data()
            .to_vec()),
    }
}

const fn is_dir(mode: u32) -> bool {
    mode & 0o170000 == 0o040000
}

const fn is_link(mode: u32) -> bool {
    mode & 0o170000 == 0o120000
}

// --- tar -----------------------------------------------------------------------------------

const BLOCK: usize = 512;
const RECORD: u64 = 10240;
const TAR_UMASK: u32 = 0o002;

struct Tar<'a> {
    out: &'a mut dyn Write,
    offset: u64,
    time: i64,
}

fn put(field: &mut [u8], text: &[u8]) {
    let n = text.len().min(field.len());
    field[..n].copy_from_slice(&text[..n]);
}

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    put(field, text.as_bytes());
}

impl Tar<'_> {
    fn write_blocked(&mut self, data: &[u8]) -> Result<()> {
        self.out.write_all(data)?;
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        if pad > 0 {
            self.out.write_all(&vec![0u8; pad])?;
        }
        self.offset += (data.len() + pad) as u64;
        Ok(())
    }

    fn header(
        &self,
        name: &[u8],
        prefix: &[u8],
        linkname: &[u8],
        mode: u32,
        size: u64,
        kind: u8,
    ) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        put(&mut h[0..100], name);
        octal(&mut h[100..108], u64::from(mode & 0o7777));
        octal(&mut h[108..116], 0);
        octal(&mut h[116..124], 0);
        octal(&mut h[124..136], size);
        octal(&mut h[136..148], self.time as u64);
        h[156] = kind;
        put(&mut h[157..257], linkname);
        put(&mut h[257..263], b"ustar\0");
        put(&mut h[263..265], b"00");
        put(&mut h[265..297], b"root");
        put(&mut h[297..329], b"root");
        octal(&mut h[329..337], 0);
        octal(&mut h[337..345], 0);
        put(&mut h[345..500], prefix);
        // The checksum is taken with the field read as eight spaces.
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].fill(0);
        octal(&mut h[148..156], u64::from(sum));
        h
    }

    fn pax_record(out: &mut Vec<u8>, keyword: &str, value: &[u8]) {
        let base = keyword.len() + value.len() + 3;
        let mut total = base;
        while base + total.to_string().len() != total {
            total = base + total.to_string().len();
        }
        out.extend_from_slice(format!("{total} {keyword}=").as_bytes());
        out.extend_from_slice(value);
        out.push(b'\n');
    }

    fn extended(&mut self, id: &str, records: &[u8]) -> Result<()> {
        let name = format!("{id}.paxheader");
        let header = self.header(
            name.as_bytes(),
            b"",
            b"",
            0o100666,
            records.len() as u64,
            b'x',
        );
        self.write_blocked(&header)?;
        self.write_blocked(records)
    }

    fn entry(&mut self, path: &[u8], mode: u32, id: &str, data: &[u8]) -> Result<()> {
        let mut records = Vec::new();
        let (kind, mode_bits) = if is_dir(mode) {
            (b'5', (mode | 0o777) & !TAR_UMASK)
        } else if is_link(mode) {
            (b'2', mode | 0o777)
        } else {
            let exec = mode & 0o100 != 0;
            (b'0', (mode | if exec { 0o777 } else { 0o666 }) & !TAR_UMASK)
        };
        let mut name = path.to_vec();
        let mut prefix: Vec<u8> = Vec::new();
        if path.len() > 100 {
            let split = path_prefix(path, 155);
            let rest = path.len() - split - 1;
            if split > 0 && rest <= 100 {
                prefix = path[..split].to_vec();
                name = path[split + 1..].to_vec();
            } else {
                name = format!("{id}.data").into_bytes();
                Self::pax_record(&mut records, "path", path);
            }
        }
        let mut linkname = Vec::new();
        if is_link(mode) {
            if data.len() > 100 {
                linkname = format!("see {id}.paxheader").into_bytes();
                Self::pax_record(&mut records, "linkpath", data);
            } else {
                linkname = data.to_vec();
            }
        }
        let regular = kind == b'0';
        let mut size_in_header = if regular { data.len() as u64 } else { 0 };
        if regular && size_in_header > 0o77777777777 {
            Self::pax_record(&mut records, "size", size_in_header.to_string().as_bytes());
            size_in_header = 0;
        }
        let header = self.header(&name, &prefix, &linkname, mode_bits, size_in_header, kind);
        if !records.is_empty() {
            self.extended(id, &records)?;
        }
        self.write_blocked(&header)?;
        if regular && !data.is_empty() {
            self.write_blocked(data)?;
        }
        Ok(())
    }
}

/// Where a long path can be cut into a `prefix` and a `name` at a `/`.
fn path_prefix(path: &[u8], max: usize) -> usize {
    let mut i = path.len();
    if i > 1 && path[i - 1] == b'/' {
        i -= 1;
    }
    if i > max {
        i = max;
    }
    loop {
        i = i.saturating_sub(1);
        if i == 0 || path[i] == b'/' {
            return i;
        }
    }
}

fn write_tar(
    repo: &Repository,
    items: &[Item],
    commit: Option<ObjectId>,
    time: i64,
    out: &mut dyn Write,
) -> Result<()> {
    let mut tar = Tar {
        out,
        offset: 0,
        time,
    };
    if let Some(commit) = commit {
        let mut records = Vec::new();
        Tar::pax_record(&mut records, "comment", commit.to_hex().as_bytes());
        let header = tar.header(
            b"pax_global_header",
            b"",
            b"",
            0o100666,
            records.len() as u64,
            b'g',
        );
        tar.write_blocked(&header)?;
        tar.write_blocked(&records)?;
    }
    for item in items {
        let data = contents(repo, &item.source)?;
        let id = match item.oid {
            Some(oid) => oid.to_string(),
            None => ObjectId::zero(repo.format()).to_string(),
        };
        tar.entry(&item.path, item.mode, &id, &data)?;
    }
    // At least two zero blocks, then pad to a whole record.
    let total = (tar.offset + 2 * BLOCK as u64).div_ceil(RECORD) * RECORD;
    let pad = (total - tar.offset) as usize;
    tar.out.write_all(&vec![0u8; pad])?;
    Ok(())
}

// --- gzip ----------------------------------------------------------------------------------

/// Gzip framing as `gzip -cn` writes it: no name, no time, Unix as the origin.
struct GzipWriter<'a> {
    out: &'a mut dyn Write,
    encoder: flate2::write::DeflateEncoder<Vec<u8>>,
    crc: flate2::Crc,
    level: u32,
    started: bool,
}

impl<'a> GzipWriter<'a> {
    fn new(out: &'a mut dyn Write, level: u32) -> Self {
        Self {
            out,
            encoder: flate2::write::DeflateEncoder::new(
                Vec::new(),
                flate2::Compression::new(level),
            ),
            crc: flate2::Crc::new(),
            level,
            started: false,
        }
    }

    fn finish(mut self) -> Result<()> {
        self.start()?;
        let size = self.crc.amount();
        let crc = self.crc.sum();
        let tail = self.encoder.finish()?;
        self.out.write_all(&tail)?;
        self.out.write_all(&crc.to_le_bytes())?;
        self.out.write_all(&size.to_le_bytes())?;
        Ok(())
    }

    fn start(&mut self) -> std::io::Result<()> {
        if !self.started {
            self.started = true;
            let xfl = match self.level {
                9 => 2,
                0 | 1 => 4,
                _ => 0,
            };
            self.out
                .write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 3])?;
        }
        Ok(())
    }
}

impl Write for GzipWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.start()?;
        self.crc.update(buf);
        self.encoder.write_all(buf)?;
        let ready = std::mem::take(self.encoder.get_mut());
        self.out.write_all(&ready)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// --- zip -----------------------------------------------------------------------------------

fn dos_time(time: i64) -> (u16, u16) {
    let zoned = jiff::Timestamp::from_second(time)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()))
        .ok();
    let Some(z) = zoned else {
        return (0, 0x21);
    };
    let year = i32::from(z.year()) - 1980;
    if year < 0 {
        return (0, 0x21);
    }
    let date = ((year as u16) << 9) | ((z.month() as u16) << 5) | z.day() as u16;
    let clock = ((z.hour() as u16) << 11) | ((z.minute() as u16) << 5) | ((z.second() as u16) / 2);
    (clock, date)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

fn deflate_raw(data: &[u8], level: u32) -> Result<Vec<u8>> {
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn write_zip(
    repo: &Repository,
    items: &[Item],
    commit: Option<ObjectId>,
    time: i64,
    level: Option<u32>,
    out: &mut dyn Write,
) -> Result<()> {
    if items.len() >= 0xffff {
        return Err(fatal("zip64 is not implemented: too many entries"));
    }
    let level = level.unwrap_or(6).min(9);
    let (dos_clock, dos_date) = dos_time(time);
    let mut offset: u64 = 0;
    let mut central: Vec<u8> = Vec::new();
    let le16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let le32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    for item in items {
        let data = contents(repo, &item.source)?;
        let directory = is_dir(item.mode);
        let link = is_link(item.mode);
        let (attr2, creator): (u32, u16) = if directory {
            (16, 0)
        } else if link {
            ((item.mode | 0o777) << 16, 0x0317)
        } else if item.mode & 0o111 != 0 {
            (item.mode << 16, 0x0317)
        } else {
            (0, 0)
        };
        let body: Vec<u8> = if directory { Vec::new() } else { data };
        let binary = directory || body.iter().take(8000).any(|b| *b == 0);
        let mut method = 0u16;
        let mut stored = body.clone();
        if !directory && !link && level != 0 && !body.is_empty() {
            let packed = deflate_raw(&body, level)?;
            if packed.len() < body.len() {
                method = 8;
                stored = packed;
            }
        }
        if body.len() >= u32::MAX as usize {
            return Err(fatal("zip64 is not implemented: member too large"));
        }
        let crc = crc32(&body);
        let flags: u16 = if item.path.is_ascii() { 0 } else { 0x0800 };
        let mut extra = Vec::new();
        le16(&mut extra, 0x5455);
        le16(&mut extra, 5);
        extra.push(1);
        le32(&mut extra, time as u32);

        let mut local = Vec::new();
        local.extend_from_slice(b"PK\x03\x04");
        le16(&mut local, 10);
        le16(&mut local, flags);
        le16(&mut local, method);
        le16(&mut local, dos_clock);
        le16(&mut local, dos_date);
        le32(&mut local, crc);
        le32(&mut local, stored.len() as u32);
        le32(&mut local, body.len() as u32);
        le16(&mut local, item.path.len() as u16);
        le16(&mut local, extra.len() as u16);
        local.extend_from_slice(&item.path);
        local.extend_from_slice(&extra);

        central.extend_from_slice(b"PK\x01\x02");
        le16(&mut central, creator);
        le16(&mut central, 10);
        le16(&mut central, flags);
        le16(&mut central, method);
        le16(&mut central, dos_clock);
        le16(&mut central, dos_date);
        le32(&mut central, crc);
        le32(&mut central, stored.len() as u32);
        le32(&mut central, body.len() as u32);
        le16(&mut central, item.path.len() as u16);
        le16(&mut central, extra.len() as u16);
        le16(&mut central, 0);
        le16(&mut central, 0);
        le16(&mut central, u16::from(!binary));
        le32(&mut central, attr2);
        le32(&mut central, offset as u32);
        central.extend_from_slice(&item.path);
        central.extend_from_slice(&extra);

        out.write_all(&local)?;
        out.write_all(&stored)?;
        offset += (local.len() + stored.len()) as u64;
    }
    let comment = commit.map(|c| c.to_hex().to_vec()).unwrap_or_default();
    out.write_all(&central)?;
    let mut trailer = Vec::new();
    trailer.extend_from_slice(b"PK\x05\x06");
    le16(&mut trailer, 0);
    le16(&mut trailer, 0);
    le16(&mut trailer, items.len() as u16);
    le16(&mut trailer, items.len() as u16);
    le32(&mut trailer, central.len() as u32);
    le32(&mut trailer, offset as u32);
    le16(&mut trailer, comment.len() as u16);
    trailer.extend_from_slice(&comment);
    out.write_all(&trailer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(keyword: &str, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Tar::pax_record(&mut out, keyword, value);
        out
    }

    #[test]
    fn pax_record_length_counts_its_own_digits() {
        // "52 comment=" + 40 hex digits + "\n" is the global header Git writes for a SHA-1 commit.
        let line = record("comment", &[b'a'; 40]);
        assert_eq!(&line[..11], b"52 comment=");
        assert_eq!(line.len(), 52);
        // Two digits up to 99 bytes; no record can be exactly 100 bytes long, so 101 follows 99.
        assert_eq!(record("path", &[b'x'; 90]).len(), 99);
        assert!(record("path", &[b'x'; 91]).starts_with(b"101 path="));
        assert_eq!(record("path", &[b'x'; 91]).len(), 101);
    }

    #[test]
    fn long_paths_split_at_a_slash_within_the_prefix_limit() {
        let path = format!("{}/{}/file", "a".repeat(80), "b".repeat(60));
        let cut = path_prefix(path.as_bytes(), 155);
        assert_eq!(
            &path.as_bytes()[..cut],
            format!("{}/{}", "a".repeat(80), "b".repeat(60)).as_bytes()
        );
        // No slash to cut at: the caller has to fall back to a pax header.
        assert_eq!(path_prefix("c".repeat(120).as_bytes(), 155), 0);
    }

    #[test]
    fn glob_lets_star_cross_slashes_and_supports_sets() {
        assert!(glob(b"*.c", b"src/deep/b.c"));
        assert!(glob(b"src/*.c", b"src/deep/b.c"));
        assert!(!glob(b"src/*.c", b"docs/a.c"));
        assert!(glob(b"[a-c]?.txt", b"b1.txt"));
        assert!(!glob(b"[!a-c]?.txt", b"b1.txt"));
    }

    #[test]
    fn specs_include_parents_of_wanted_paths_and_honour_exclusions() {
        let specs = parse_specs(&[BString::from("src/deep"), BString::from("docs/d.txt")]).unwrap();
        let mut hit = vec![false; specs.len()];
        assert!(interest(&specs, &mut hit, b"src", true, false) == Interest::Descend);
        assert!(interest(&specs, &mut hit, b"src/deep", true, false) == Interest::All);
        assert!(interest(&specs, &mut hit, b"src/a.c", false, false) == Interest::No);
        assert!(interest(&specs, &mut hit, b"docs/d.txt", false, false) == Interest::All);
        assert_eq!(hit, vec![true, true]);
        let specs = parse_specs(&[BString::from(":!docs")]).unwrap();
        let mut hit = vec![false; 1];
        assert!(interest(&specs, &mut hit, b"docs", true, false) == Interest::No);
        assert!(interest(&specs, &mut hit, b"src", true, false) == Interest::All);
    }

    #[test]
    fn dos_time_packs_date_and_time_fields() {
        // Only meaningful when the zone is UTC, which the differential harness fixes.
        if std::env::var("TZ").as_deref() == Ok("UTC") {
            let (clock, date) = dos_time(1_700_000_000);
            assert_eq!(date, (43 << 9) | (11 << 5) | 14);
            assert_eq!(clock, (22 << 11) | (13 << 5) | 10);
        }
    }
}
