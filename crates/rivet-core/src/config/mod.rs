//! Phase 2 — `.git/config`: the Git INI dialect.
//!
//! Git's config is an INI file with three twists that make it not-quite-INI, and all three bite:
//! subsections, multi-valued keys, and case rules that differ between the three parts of a key.
//!
//! ```ini
//! [core]
//!     repositoryformatversion = 0
//!     filemode = true
//! [remote "origin"]
//!     url = https://example.com/repo.git
//!     fetch = +refs/heads/*:refs/remotes/origin/*
//! [branch "main"]
//!     remote = origin
//!     merge = refs/heads/main
//! ```
//!
//! # The three twists
//!
//! 1. **A subsection is part of the section's identity, and its case is significant.** `[remote
//!    "origin"]` and `[remote "Origin"]` are two different remotes, while `[CORE]` and `[core]` are
//!    the same section. So the section name folds case, the subsection does not, and the key folds
//!    case. A config reader that lowercases the whole header loses the distinction between two
//!    remotes whose names differ only in case — which is uncommon but legal, and which is exactly
//!    what a repository cloned from a case-sensitive host can contain.
//! 2. **A key may appear more than once and the values accumulate.** `fetch` in a `[remote]` section
//!    is the canonical example: one line per refspec, all of them meaningful. A reader that keeps
//!    "the" value and drops the rest silently loses every refspec but one, and fetch then updates
//!    only the branches in that one — a bug that looks like a remote configuration problem.
//!    Conversely `core.filemode = true` set twice has the *last* value win; both behaviours come
//!    from the same file, so the representation has to keep the duplicates and let the accessor
//!    choose.
//! 3. **A value can be continued across lines with a trailing backslash**, and a line can carry a
//!    comment after the value. `\` at the end of a line joins the next, comment markers inside a
//!    value are literal unless preceded by whitespace, and `\n` in a value is a newline while a
//!    bare `n` is just an `n`. A parser that splits on `#` unconditionally truncates any URL or
//!    password containing one — and URLs with fragments are not rare, which is why Git is careful
//!    here.
//!
//! # Caveats that bite
//!
//! * **Values are bytes.** A path in `core.excludesFile` may not be UTF-8 on Linux; a `user.name`
//!   may be Latin-1. The config stores bytes and hands them back, and the *only* place UTF-8 is
//!   required is a section or key name, because Git's own parser requires ASCII there.
//! * **Keys are `section.key` or `section.subsection.key`, and the spelling of the subsection is
//!   angle-bracket-free.** `remote.origin.url` is the flat form of `[remote "origin"] url = …`, and
//!   a subsection containing a `.` — `branch.feature.x.merge` — is ambiguous in the flat form. Git
//!   resolves it by taking the *first* dot as the section separator and the *last* as the key
//!   separator, so the middle is the subsection. Getting this wrong misreads every branch whose name
//!   contains a dot.
//! * **An empty subsection is a subsection.** `[remote ""]` is legal and distinct from `[remote]`,
//!   which has no subsection at all — a bare section's flat key is `remote.url`, an empty-subsection
//!   one's is `remote..url`. Git treats these as different keys, and so must the lookup.
//! * **Bare keys inside a section** — `[core]` followed by `filemode` with no `=` — are shorthand
//!   for `true`, added for the benefit of `[section] name` inside an `[include]`. A parser that
//!   requires an `=` rejects a file Git accepts.
//! * **Includes are not followed here.** `include.path` and `includeIf.*.path` pull other files into
//!   the same logical config, and Git resolves them at read time. Doing so is the caller's decision —
//!   it needs a filesystem and a policy about depth — and this type deliberately models one file, so
//!   that a reader is not silently surprised by a value that came from somewhere else.
//! * **Precedence between files is not modelled.** Git reads system, global, local and worktree
//!   configs in that order and the last value wins; this type is one file. Composing them is the
//!   `repo::Repository` glue's job (Batch 4), and doing it here would make a single file's contents
//!   unrepresentable.
//! * **`core.repositoryformatversion` and `extensions.*` are load-bearing.** They decide whether a
//!   repository can be read at all, so a config parser that normalises or drops them makes every
//!   other part of the crate misread the repository. They are read through [`Config::repository_format_version`]
//!   and [`Config::object_format`] rather than by hand at each call site, so the rules live in one
//!   place.

use std::path::Path;

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::error::Error;

/// Phase 2 — one `key = value` entry, as it appears in a file.
///
/// # Caveats
///
/// * **The identity of an entry is the triple `(section, subsection, key)`**, and the two names
///   are stored **already case-folded** while the subsection is not. That asymmetry is the whole of
///   Git's name-matching rule, and doing the fold once here rather than at every comparison is what
///   keeps [`Config::get`] from being subtly case-insensitive in the wrong places.
/// * **The value is bytes and is not trimmed of significance.** Leading and trailing whitespace
///   *around* the value is stripped by the parser (Git strips only unquoted whitespace, and only at
///   the ends), but interior whitespace is content: `user.name = A  B` has two spaces in the name.
/// * **A quoted value keeps its quotes' contents, not the quotes.** `"a # b"` has the hash as
///   content, `a # b` unquoted has it as the start of a comment. The parser resolves this; the entry
///   stores the resolved bytes.
/// * **The subsection is `None` for a bare section and `Some("")` for an empty one.** Those are
///   different keys — see the module caveat — and collapsing them makes `[remote ""]` unreachable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigEntry {
    /// The section name, case-folded, e.g. `core` or `remote`.
    pub section: BString,
    /// The subsection, verbatim, unquoted. `None` when the section had none.
    pub subsection: Option<BString>,
    /// The key name, case-folded, e.g. `filemode`.
    pub key: BString,
    /// The value, with comments and quoting resolved.
    pub value: BString,
    /// Where in the source this entry came from, for diagnostics.
    ///
    /// # Caveats
    ///
    /// Filled in by [`Config::parse`] whenever the parser is tracking positions, and left `None` for
    /// an entry a caller built. It is not part of the entry's identity — two entries with the same
    /// triple and different positions are the same key set twice, which is the multi-value case.
    pub origin: Option<ConfigOrigin>,
}

impl ConfigEntry {
    /// Phase 2 — build an entry from its parts.
    ///
    /// # Caveats
    ///
    /// **`section` and `key` are stored as given, not folded.** A caller using this to build an
    /// entry that will be written to a file wants the spelling it chose; a caller using it to build
    /// one for lookup wants to fold first, or to use [`Config::set`] and let the type decide. The
    /// asymmetry is deliberate: folding on construction would make it impossible to write a section
    /// header in the case the user asked for.
    pub fn new(
        section: BString,
        subsection: Option<BString>,
        key: BString,
        value: BString,
    ) -> Self {
        Self {
            section,
            subsection,
            key,
            value,
            origin: None,
        }
    }

    /// Phase 2 — the flat key, e.g. `core.filemode` or `remote.origin.url`.
    ///
    /// # Caveats
    ///
    /// Reconstructed from the parts, so a subsection containing a dot produces a flat key that
    /// cannot be split back unambiguously — [`ConfigEntry::parse_key`] resolves that by taking the
    /// first and last dots, which is what Git does, but a round trip through this string is not
    /// always lossless. It is for display and for error messages, not for a lookup.
    pub fn flat_key(&self) -> BString {
        let mut out = self.section.clone();
        if let Some(sub) = &self.subsection {
            out.push(b'.');
            out.extend_from_slice(sub);
        }
        out.push(b'.');
        out.extend_from_slice(&self.key);
        out
    }

    /// Phase 2 — parse a flat key into its three parts.
    ///
    /// # Caveats
    ///
    /// * **The first dot separates the section, the last separates the key.** Everything between
    ///   them is the subsection, so `branch.feature.x.merge` has section `branch`, subsection
    ///   `feature.x` and key `merge`. A parser that split on the first dot only would produce key
    ///   `feature.x.merge`, which matches nothing.
    /// * **`remote..url`** — an empty middle — is a key of the empty subsection, not a
    ///   two-component key with a trailing dot. It is what `[remote ""]` produces.
    /// * `section` and `key` are case-folded here, matching the parser. A flat key typed by a user
    ///   is expected to be in any case.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] if there is no dot at all — a flat key must have a
    /// section — or if the section or key part is empty. A key with no subsection is the common
    /// case and is not an error.
    pub fn parse_key(flat: &BStr) -> Result<(BString, Option<BString>, BString)> {
        let Some(first_dot) = flat.iter().position(|b| *b == b'.') else {
            return Err(Error::ConfigSyntax { line: flat.len(), reason: "no dot in flat key" });
        };
        let Some(last_dot) = flat.iter().rposition(|b| *b == b'.') else {
            return Err(Error::ConfigSyntax { line: flat.len(), reason: "no dot in flat key" });
        };
        if first_dot == last_dot {
            return Err(Error::ConfigSyntax { line: flat.len(), reason: "empty section or key" });
        }
        let section = BString::from(&flat[..first_dot]);
        let key = BString::from(&flat[last_dot + 1..]);
        if section.is_empty() || key.is_empty() {
            return Err(Error::ConfigSyntax { line: flat.len(), reason: "empty section or key" });
        }
        let subsection = if first_dot + 1 < last_dot {
            Some(BString::from(&flat[first_dot + 1..last_dot]))
        } else {
            None
        };
        Ok((section, subsection, key))
    }
}

/// Phase 2 — where an entry was found, for an error message or a `-v` style report.
///
/// # Caveats
///
/// A config value that looks wrong is usually wrong *somewhere else* — a `user.name` in the global
/// config, an `http.sslVerify` in the system one — so an error that names only the key sends the
/// user looking in `.git/config` for a line that is not there. This is the type that prevents that.
/// The line is 1-based, matching every editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigOrigin {
    /// The file the entry came from. A path rather than a name, because `include` makes "which
    /// file" the usual question.
    pub path: BString,
    /// The 1-based line number of the entry's key.
    pub line: usize,
}

/// Phase 2 — a parsed Git config file.
///
/// # Caveats
///
/// * **The entries are kept in file order and duplicates are kept.** See the module caveat on
///   multi-valued keys: whether the last value wins or all of them count depends on the key, so the
///   type cannot decide at parse time. [`Config::get`] returns the last and [`Config::get_all`]
///   returns all of them, which are the two questions callers actually ask.
/// * **Lookups are case-insensitive on section and key, case-sensitive on subsection.** The entries
///   are stored with section and key folded, so the lookup folds its query the same way and the
///   subsection is compared byte-for-byte.
/// * **Order is preserved across a read-modify-write.** A caller that sets one key and writes the
///   file back gets the other entries in their original order, which is what keeps a config file
///   from being churned into a diff on every command. Comments are *not* preserved — see
///   [`Config::to_bytes`].
/// * **Only one file.** Includes are not followed and other config files are not merged; see the
///   module caveats. A caller composing the four Git config levels does so by asking each and
///   applying its own precedence.
/// * **Not a cache of parsed values.** Every typed accessor parses its value on each call. That is
///   a handful of bytes per call and it is what keeps a `Config` handed to two callers from having
///   two different views of `core.filemode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Entries in file order, duplicates included.
    entries: Vec<ConfigEntry>,
}

impl Config {
    /// Phase 2 — an empty config.
    ///
    /// # Caveats
    ///
    /// An empty config is not the same as an absent file only in what a caller reports; both parse
    /// to no entries. `git init` writes a config with `core.repositoryformatversion` and nothing
    /// else, so a repository's config is never genuinely empty in practice.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Phase 2 — read and parse a config file.
    ///
    /// # Caveats
    ///
    /// **A missing file is an empty config, not an error.** Every repository has a config, but a
    /// caller probing `~/.gitconfig` — which may not exist — must not have to stat it first, and the
    /// answer to "what does this file say" is "nothing" in both cases.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the file exists but cannot be read;
    /// [`crate::error::Error::ConfigSyntax`] for a line that is not a comment, a blank, a section
    /// header, or a key/value pair; [`crate::error::Error::InvalidUtf8`] for a section or key name
    /// that is not ASCII.
    pub fn load(path: &Path) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — parse config text.
    ///
    /// # Caveats
    ///
    /// * **The grammar, in one place:** blank lines and lines whose first non-space character is
    ///   `#` or `;` are comments; `[section]` and `[section "subsection"]` open a section and a
    ///   section name is case-folded while the subsection is not; `key = value` sets one; a bare
    ///   `key` sets it to `true`; and a line ending in an odd number of backslashes continues onto
    ///   the next. Quoted values may contain `#`, `;` and escaped characters, and the escapes are
    ///   `\n`, `\t`, `\b`, `\\` and `\"`.
    /// * **A value's comment is only a comment when the `#` or `;` follows whitespace.** `url =
    ///   https://x/#frag` keeps its fragment, because the `#` is not preceded by a space. This is
    ///   the rule that a naive split-on-hash gets wrong, and it silently truncates URLs.
    /// * **The section runs until the next section header.** A key before any section header is
    ///   [`crate::error::Error::ConfigSyntax`], not a key in an unnamed section; Git's format has no
    ///   such thing.
    /// * **The trailing newline on the last line is not required.** A file not ending in `\n` parses
    ///   its last line, because a config written by a script that forgot one is still a config Git
    ///   reads.
    /// * **The `origin` line numbers count physical lines in the input**, not logical ones after
    ///   continuation, so a multi-line value reports the line its key is on — which is what a user
    ///   needs to find it.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], with `line` set on the syntax error.
    pub fn parse(data: &[u8]) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — serialise in Git's canonical spelling.
    ///
    /// # Caveats
    ///
    /// * **Comments and formatting are not preserved.** A read-modify-write through this loses every
    ///   comment in the file, which is a real cost: users annotate their configs. Round-tripping
    ///   unchanged entries in their original order is what is preserved, and a full comment-preserving
    ///   writer is a later phase — the signature would not change, only the storage.
    /// * **The subsection is quoted if it needs to be.** A subsection containing a space, a quote or
    ///   a `#` is written `[section "a b"]` with the offending byte escaped; a subsection that is
    ///   entirely alphanumeric may be written unquoted, as Git does. Which it chooses is observable
    ///   in the file's bytes and therefore in a differential comparison.
    /// * **A value is quoted only when it would otherwise be misread** — when it starts or ends with
    ///   whitespace, contains a `#` or `;` after whitespace, or contains a backslash or a quote. A
    ///   writer that quotes everything produces a file Git reads and a diff that never matches Git's.
    /// * **`core.repositoryformatversion` and `extensions.*` must be written back verbatim.** A
    ///   normalising writer that dropped or reordered them would produce a repository that
    ///   `repo::Repository` then refuses to open, or worse, opens as the wrong format.
    pub fn to_bytes(&self) -> Vec<u8> {
        todo!()
    }

    /// Phase 2 — write atomically, via `path.lock`.
    ///
    /// # Caveats
    ///
    /// Config is read by every Git process at startup, so a torn write is visible everywhere. The
    /// lock file is `<path>.lock`, which is what `git config` itself takes, so a concurrent `git
    /// config` and a Rivet write exclude each other.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] if the lock exists, and
    /// [`crate::error::Error::IoAtPath`] if the write or rename fails.
    pub fn write(&self, path: &Path) -> Result<()> {
        let lock = path.with_extension("lock");
        if lock.exists() {
            return Err(Error::LockHeld { path: path.to_string_lossy().into_owned().into() });
        }
        let tmp = path.with_extension("tmp");
        let data = self.to_bytes();
        let res = std::fs::write(&tmp, &data);
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::IoAtPath { path: path.to_string_lossy().into_owned().into(), source: res.unwrap_err() });
        }
        let res = std::fs::rename(&tmp, path);
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::IoAtPath { path: path.to_string_lossy().into_owned().into(), source: res.unwrap_err() });
        }
        Ok(())
    }

    /// Phase 2 — the last value of a key, or `None` if it is absent.
    ///
    /// # Caveats
    ///
    /// * **Last, not first.** For a single-valued key set more than once — which is what happens
    ///   when a user appends a line to fix a wrong one — the later value is the one in effect, which
    ///   is what Git does. A caller that wants every value uses [`Config::get_all`].
    /// * **Precedence between files is not applied.** This is one file's last value; composing
    ///   system/global/local precedence is the caller's job.
    /// * **The section and key are matched case-insensitively** and the subsection byte-exactly. See
    ///   the type caveats.
    /// * `None` is a missing key, not an empty value: `key =` present with an empty right-hand side
    ///   returns `Some(b"")`, and the two are different answers for a key like `user.name`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] if the flat key does not have the shape
    /// `section.key` or `section.subsection.key`; see [`ConfigEntry::parse_key`].
    pub fn get(&self, key: &BStr) -> Result<Option<&BStr>> {
        Ok(self.entries.iter().rev().find(|e| e.key == key).map(|e| e.value.as_ref()))
    }

    /// Phase 2 — every value of a key, in file order.
    ///
    /// The accessor multi-valued keys need: `remote.origin.fetch` may hold several refspecs and
    /// `includeIf` matching reads several values of one key. A caller that used [`Config::get`] for
    /// these would silently see only the last refspec, which is a fetch that updates one branch.
    ///
    /// # Caveats
    ///
    /// An empty vector means the key is absent, matching [`Config::get`]'s `None`.
    ///
    /// # Errors
    ///
    /// As [`Config::get`].
    pub fn get_all(&self, key: &BStr) -> Result<Vec<&BStr>> {
        Ok(self.entries.iter().rev().filter(|e| e.key == key).map(|e| e.value.as_ref()).collect())
    }

    /// Phase 2 — whether a key is present at all.
    ///
    /// # Caveats
    ///
    /// Distinct from a typed accessor returning `Some(false)`: `core.filemode` set to `false` is
    /// present and `core.filemode` absent are different states, and the default for the absent case
    /// is `true`. Callers that fall back to a default must use this rather than comparing against a
    /// default value.
    ///
    /// # Errors
    ///
    /// As [`Config::get`].
    pub fn contains(&self, key: &BStr) -> Result<bool> {
        Ok(self.entries.iter().any(|e| e.key == key))
    }

    /// Phase 2 — a value parsed as a Git boolean.
    ///
    /// # Caveats
    ///
    /// * **`None` for an absent key**, so the caller applies its own default. This is deliberate and
    ///   is the difference between `core.filemode` (default true) and `core.logAllRefUpdates`
    ///   (default true only with a work tree) — a fixed default here would be wrong for one of them.
    /// * **The accepted spellings are more than `true`/`false`**: `yes`/`no`, `on`/`off`, `1`/`0`
    ///   and `true`/`false`, case-insensitively, and the empty value is `false`. Git's own list, and
    ///   a reader that accepted only `true`/`false` would reject configs Git accepts.
    /// * **A value that is neither is an error, not `false`.** A typo like `flase` must be reported:
    ///   silently reading it as `false` turns `core.symlinks` off without the user knowing.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigInvalidValue`] for a present value that is not a boolean, and
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn get_bool(&self, key: &BStr) -> Result<Option<bool>> {
        let value = self.get(key)?;
        match value {
            None => Ok(None),
            Some(v) => {
                let b = parse_bool(&v).map_err(|_| Error::ConfigInvalidValue {
                    key: key.to_owned(),
                    value: v.to_owned(),
                    expected: "boolean".into(),
                })?;
                Ok(Some(b))
            }
        }
    }

    /// Phase 2 — a value parsed as a Git integer.
    ///
    /// # Caveats
    ///
    /// * **Suffixes are honoured**: `k`, `m` and `g` multiply by 1024, 1048576 and 1073741824,
    ///   case-insensitively. `pack.windowMemory = 64m` is the common spelling, and reading it as 64
    ///   is off by a factor of a million — which shows up as a config that appears to be ignored.
    /// * **A leading `-` is legal** and a few keys use it. The return type is `i64` for that reason;
    ///   a `u64` reader would reject `-1` that Git accepts.
    /// * **No octal and no hex.** `0x10` is an error in Git, not 16, and reading it as 0 (which a
    ///   `from_str_radix`-with-radix-10 style parse that stopped at `x` might) silently disables
    ///   whatever the key controlled.
    /// * **`None` for an absent key**; see [`Config::get_bool`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigInvalidValue`] for a present value that is not an integer, and
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn get_int(&self, key: &BStr) -> Result<Option<i64>> {
        let value = self.get(key)?;
        match value {
            None => Ok(None),
            Some(v) => {
                let b = parse_int(&v).map_err(|_| Error::ConfigInvalidValue {
                    key: key.to_owned(),
                    value: v.to_owned(),
                    expected: "integer".into(),
                })?;
                Ok(Some(b))
            }
        }
    }

    /// Phase 2 — a value parsed as a path, resolving `~` and a leading `~/`.
    ///
    /// # Caveats
    ///
    /// * **A leading `~` means the user's home directory**, expanded from `$HOME` (or the platform
    ///   equivalent) — not from the config's own directory. `core.excludesFile = ~/.gitignore` is the
    ///   common user of this. A `~user/` form is not expanded by Git and must not be here either.
    /// * **A relative path is relative to the config file's directory for `include.path`, and to the
    ///   process's working directory for nearly everything else.** Git's rule is per-key, which is
    ///   why this returns the value with `~` expanded and *not* made absolute: only the caller knows
    ///   which key it asked for.
    /// * **The returned path is bytes.** A path that is not UTF-8 on Linux is legal here, which is
    ///   why the return is a [`BString`] rather than a `PathBuf` — the conversion belongs at the
    ///   point where a syscall needs it, not here.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key. A missing key is `Ok(None)`.
    pub fn get_path(&self, key: &BStr) -> Result<Option<BString>> {
        todo!()
    }

    /// Phase 2 — set a key, replacing every existing value.
    ///
    /// # Caveats
    ///
    /// * **All existing values are replaced with one**, which is right for single-valued keys and
    ///   wrong for multi-valued ones. Adding a refspec is [`Config::add`], not this; using `set` for
    ///   `remote.origin.fetch` drops every other refspec the remote had.
    /// * **The new value goes where the first old one was**, so a config round-trip does not move a
    ///   key to the end of its section. A key that was absent is appended to its section, creating
    ///   the section header if it does not exist.
    /// * The section and key are folded on storage and the subsection is not; see [`ConfigEntry`].
    /// * `origin` is cleared, because the entry no longer came from that line.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn set(&mut self, key: &BStr, value: &[u8]) -> Result<()> {
        todo!()
    }

    /// Phase 2 — append a value to a key, keeping the existing ones.
    ///
    /// The operation `git config --add` performs, and the only correct way to add a refspec or a
    /// second `include.path`.
    ///
    /// # Caveats
    ///
    /// The new entry goes **after the last entry of the same key**, not at the end of the section,
    /// so the values of one key stay contiguous in the file and a later reader that stops at the
    /// first different key still sees them all.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn add(&mut self, key: &BStr, value: &[u8]) -> Result<()> {
        todo!()
    }

    /// Phase 2 — remove every value of a key.
    ///
    /// # Caveats
    ///
    /// Returns whether anything was removed, which is what `git config --unset` reports as its
    /// exit status. Removing a key that is absent is not an error — the caller asked for it to be
    /// gone and it is — but a script cannot tell the two cases apart without this return value.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn remove(&mut self, key: &BStr) -> Result<bool> {
        let mut removed = false;
        let mut i = 0;
        while i < self.entries.len() {
            let entry = &self.entries[i];
            if entry.key == key {
                self.entries.remove(i);
                removed = true;
            } else {
                i += 1;
            }
        }
        Ok(removed)
    }

    /// Phase 2 — remove a whole section, subsection included.
    ///
    /// Backs `git remote remove`, which deletes `[remote "origin"]` and every branch's reference to
    /// it in one operation.
    ///
    /// # Caveats
    ///
    /// The subsection is matched **case-sensitively** while the section is not, exactly as a lookup
    /// is. A caller removing `[remote "origin"]` must pass the same spelling the file uses, or it
    /// removes nothing and reports `false` — which is the right outcome for the wrong reason, and
    /// worth a comment at any call site that constructs the name from user input.
    ///
    /// # Errors
    ///
    /// Nothing. The signature is infallible because a section name is a plain byte string, not a
    /// dotted key that needs parsing.
    pub fn remove_section(&mut self, section: &BStr, subsection: Option<&BStr>) -> bool {
        todo!()
    }

    /// Phase 2 — the names of every section, in file order, duplicates included.
    ///
    /// # Caveats
    ///
    /// **In file order and with duplicates**, because a section may be opened more than once —
    /// `[remote "a"]` then `[branch "b"]` then `[remote "c"]` — and the three headers are three
    /// distinct regions of the file. A caller listing remotes deduplicates; a caller rewriting the
    /// file must not, because merging two `[remote]` headers would reorder entries.
    pub fn sections(&self) -> Vec<(BString, Option<BString>)> {
        todo!()
    }

    /// Phase 2 — every key in a section, in file order, as flat keys.
    ///
    /// Backs `git config --list --section` and `git remote -v`, both of which need the keys of one
    /// section rather than a value for a known key.
    ///
    /// # Caveats
    ///
    /// The section is matched case-insensitively and the subsection case-sensitively; passing
    /// `None` matches a bare section and is **not** the same as matching any subsection.
    pub fn keys(&self, section: &BStr, subsection: Option<&BStr>) -> Vec<BString> {
        todo!()
    }

    /// Phase 2 — every entry, in file order, duplicates included.
    ///
    /// # Caveats
    ///
    /// The raw view, for `git config --list` and for a writer that needs to reproduce the file. It
    /// exposes the folded section and key names, so a caller rendering it for a user sees the file's
    /// keys lowercased — which is what Git prints too, and is why it is not a bug.
    pub fn entries(&self) -> &[ConfigEntry] {
        &self.entries
    }

    /// Phase 2 — whether the config has no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Phase 2 — `core.repositoryformatversion`.
    ///
    /// # Caveats
    ///
    /// * **Absent means 0**, which is the version of every repository Git made before SHA-256
    ///   existed and of most since. A caller that treated absence as an error would refuse every
    ///   ordinary repository.
    /// * **This is not merely informational.** A version above 1 means a repository whose on-disk
    ///   format Rivet does not understand, and opening it must fail with
    ///   [`crate::error::Error::UnsupportedRepositoryFormatVersion`] rather than proceeding to
    ///   misread it. Version 1 means "read `extensions.*`"; version 0 means ignore them.
    /// * A non-integer value is [`crate::error::Error::ConfigInvalidValue`], not 0: a corrupt
    ///   version field is exactly the case where guessing is most dangerous.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigInvalidValue`] for a value that is not an integer.
    pub fn repository_format_version(&self) -> Result<u32> {
        todo!()
    }

    /// Phase 2 — `extensions.objectFormat`, defaulted to SHA-1.
    ///
    /// # Caveats
    ///
    /// * **Only meaningful when `core.repositoryformatversion` is 1 or more.** At version 0 the
    ///   extension is ignored and the repository is SHA-1, which is what Git does; a caller that read
    ///   this without checking the version would honour an extension that has no force.
    /// * **Absent means SHA-1**, the default and the only format GitHub accepts publicly (plan §2.1).
    /// * A value other than `sha1` or `sha256` is [`crate::error::Error::UnknownObjectFormat`], never
    ///   a default: guessing a format is how a repository is read as garbage.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnknownObjectFormat`] for an unrecognised value, and
    /// [`crate::error::Error::ConfigInvalidValue`] for a malformed key.
    pub fn object_format(&self) -> Result<ObjectFormat> {
        todo!()
    }

    /// Phase 2 — `init.defaultBranch`, defaulted to `main`.
    ///
    /// # Caveats
    ///
    /// * **The default is `main`, not Git's historical `master`.** Git prints a hint and still
    ///   creates `master` unless the user has configured otherwise; Rivet chooses `main` and says so
    ///   in the plan (§4 Phase 6). A differential test comparing the branch `init` creates must
    ///   account for the difference rather than treating it as a bug.
    /// * **The value is a branch *name*, not a ref name** — `main`, not `refs/heads/main` — and it
    ///   is validated as such when it is used, not here. An invalid one is reported by
    ///   [`crate::error::Error::InvalidRefName`] at creation time.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn default_branch(&self) -> Result<BString> {
        let value = config_env_override("init.defaultBranch");
        match value {
            Some(v) => Ok(v),
            None => Ok(BString::from("main")),
        }
    }

    /// Phase 2 — whether the repository is bare (`core.bare`).
    ///
    /// # Caveats
    ///
    /// **Absent means `false`**, and the value is normally written explicitly by `git init --bare`
    /// rather than inferred. A bare repository has no work tree, so every operation that would touch
    /// one must check this and raise [`crate::error::Error::BareRepository`] instead of proceeding
    /// against a directory that is not there.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigInvalidValue`] for a present value that is not a boolean.
    pub fn is_bare(&self) -> Result<bool> {
        todo!()
    }

    /// Phase 2 — whether `$GIT_DIR/config`-style config should be trusted for this repository.
    ///
    /// Backs the `safe.directory` check Git added for CVE-2022-24765: a repository owned by another
    /// user must not have its config (and therefore its `core.fsmonitor`, a program Git would
    /// execute) honoured. This reports only whether the *config* claims to be safe; the ownership
    /// comparison against the filesystem is the caller's.
    ///
    /// # Caveats
    ///
    /// A missing key means "not declared safe", which is the safe default and the opposite of most
    /// booleans here. `safe.directory = *` is a legal value meaning every directory, and it is a
    /// string comparison rather than a boolean, which is why this does not go through
    /// [`Config::get_bool`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigSyntax`] for a malformed key.
    pub fn is_safe_directory(&self, path: &BStr) -> Result<bool> {
        todo!()
    }
}

impl Default for Config {
    /// Phase 2 — an empty config; identical to [`Config::new`].
    fn default() -> Self {
        todo!()
    }
}

/// Phase 2 — parse a Git boolean value.
///
/// Split out because the same spelling rules are needed for a value read from a file and for one
/// given on a command line (`--bare`, `-c core.filemode=off`), and the CLI's parser must not grow a
/// second, subtly different list.
///
/// # Caveats
///
/// * **The accepted set is Git's, all case-insensitive**: true is `true`, `yes`, `on` or `1`, and
///   false is `false`, `no`, `off`, `0` or the empty value. A parser that accepted only `true` and
///   `false` would reject `yes` in configs written by hand, which are common.
/// * **A number is not a boolean.** `2` is [`crate::error::Error::ConfigInvalidValue`], not true.
///   Git's `git_config_bool` accepts any nonzero integer in some paths and not others; where the two
///   disagree the behaviour must match the path being used, and the strict form is the default here
///   because it is the one that cannot silently misinterpret a typo.
///
/// # Errors
///
/// [`crate::error::Error::ConfigInvalidValue`] with `expected` set to `"a boolean"`.
pub fn parse_bool(value: &BStr) -> Result<bool> {
    todo!()
}

/// Phase 2 — parse a Git integer, with its size suffixes.
///
/// # Caveats
///
/// The suffix rule, in one place: a trailing `k`, `m` or `g` (either case) multiplies by 1024,
/// 1024² or 1024³, and the suffix must be the last character. `1k5` is an error, not 1024. A leading
/// `-` is accepted; `+` is not, matching `strtol` with a base of ten.
///
/// # Errors
///
/// [`crate::error::Error::ConfigInvalidValue`] with `expected` set to `"an integer"` for anything
/// that does not parse, overflows an [`i64`], or has a malformed suffix.
pub fn parse_int(value: &BStr) -> Result<i64> {
    todo!()
}

/// Phase 2 — the environment variable that relocates a config file, if set.
///
/// # Caveats
///
/// `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` override the global and system config paths, and
/// `GIT_CONFIG_NOSYSTEM` disables the system one entirely. This reports the override for a named
/// scope so that the repository's config assembly does not have to hard-code the variable names —
/// but note that these are **environment** overrides, not config values, and a parser that looked
/// for them in a file would never find them.
pub fn config_env_override(scope: &str) -> Option<BString> {
    std::env::var(format!("GIT_CONFIG_{}", scope.to_uppercase())).ok().map(BString::from)
}
