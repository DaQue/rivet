//! Attributes and line-ending conversion: what makes a work tree file and its blob differ.
//!
//! Git does not store a work tree file as it finds it. `add` runs the *clean* conversion
//! (work tree → blob) and `checkout` runs the *smudge* conversion (blob → work tree); the pair is
//! decided per path by the attribute stack (`.gitattributes` files, `$GIT_DIR/info/attributes`,
//! `core.attributesFile`) and by `core.autocrlf` / `core.eol` / `core.safecrlf`.
//!
//! # Caveats that bite
//!
//! * **The attribute precedence is the reverse of the read order.** Lowest to highest:
//!   `core.attributesFile`, the root `.gitattributes`, each deeper directory's, then
//!   `info/attributes`; inside one file the last matching line wins.
//! * **`text=auto` looks at the content.** A file with a NUL, a lone CR or mostly non-printable
//!   bytes is binary and is never converted; a text file whose *index* blob already holds a CR
//!   is left alone too (git's "safer autocrlf"), so the clean conversion needs to know that.
//! * **Only CR directly before LF is removed** on the way in, and only a LF without a CR in
//!   front of it gains one on the way out.
//! * **What cannot be honoured is refused.** `ident`, `working-tree-encoding` and a `filter=`
//!   driver that is configured would change the bytes; they are refused by name rather than
//!   producing a blob git would not have written.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use bstr::{BStr, BString, ByteSlice};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::ignore::{IgnoreSource, PathKind, Pattern};

/// The state of one attribute for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attr {
    /// No rule mentions it (or `!name` reset it).
    Unspecified,
    /// `name`.
    Set,
    /// `-name`.
    Unset,
    /// `name=value`.
    Value(BString),
}

impl Attr {
    /// The word `check-attr` prints for this state.
    pub fn render(&self) -> BString {
        match self {
            Attr::Unspecified => BString::from("unspecified"),
            Attr::Set => BString::from("set"),
            Attr::Unset => BString::from("unset"),
            Attr::Value(v) => v.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoCrlf {
    False,
    True,
    Input,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SafeCrlf {
    Off,
    Warn,
    Die,
}

/// The configuration the conversion reads, taken once when the work tree handle is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvertConfig {
    autocrlf: AutoCrlf,
    eol_crlf: bool,
    safecrlf: SafeCrlf,
    attributes_file: Option<PathBuf>,
    clean_filters: BTreeSet<BString>,
    smudge_filters: BTreeSet<BString>,
}

impl ConvertConfig {
    /// Read `core.autocrlf`, `core.eol`, `core.safecrlf`, `core.attributesFile` and the names of
    /// the configured `filter.<name>.*` drivers.
    ///
    /// # Errors
    ///
    /// [`Error::ConfigInvalidValue`] for a boolean that is not one.
    pub fn from_config(config: &Config) -> Result<Self> {
        let autocrlf = match config.get(BStr::new(b"core.autocrlf"))? {
            Some(v) if v.eq_ignore_ascii_case(b"input") => AutoCrlf::Input,
            Some(_) => {
                if config
                    .get_bool(BStr::new(b"core.autocrlf"))?
                    .unwrap_or(false)
                {
                    AutoCrlf::True
                } else {
                    AutoCrlf::False
                }
            }
            None => AutoCrlf::False,
        };
        let eol_crlf = config
            .get(BStr::new(b"core.eol"))?
            .is_some_and(|v| v.eq_ignore_ascii_case(b"crlf"));
        let safecrlf = match config.get(BStr::new(b"core.safecrlf"))? {
            Some(v) if v.eq_ignore_ascii_case(b"warn") => SafeCrlf::Warn,
            Some(_) => {
                if config
                    .get_bool(BStr::new(b"core.safecrlf"))?
                    .unwrap_or(false)
                {
                    SafeCrlf::Die
                } else {
                    SafeCrlf::Off
                }
            }
            None => SafeCrlf::Warn,
        };
        let attributes_file = match config.get_path(BStr::new(b"core.attributesFile"))? {
            Some(p) => Some(PathBuf::from(OsStr::from_bytes(&p))),
            None => default_attributes_file(),
        };
        let mut clean_filters = BTreeSet::new();
        let mut smudge_filters = BTreeSet::new();
        for (section, sub) in config.sections() {
            if !section.eq_ignore_ascii_case(b"filter") {
                continue;
            }
            let Some(name) = sub else { continue };
            for key in config.keys(BStr::new(b"filter"), Some(BStr::new(&name))) {
                let key = key.to_ascii_lowercase();
                if key == b"clean" || key == b"process" {
                    clean_filters.insert(name.clone());
                }
                if key == b"smudge" || key == b"process" {
                    smudge_filters.insert(name.clone());
                }
            }
        }
        Ok(Self {
            autocrlf,
            eol_crlf,
            safecrlf,
            attributes_file,
            clean_filters,
            smudge_filters,
        })
    }
}

fn default_attributes_file() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("git/attributes"));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".config/git/attributes"))
}

struct Rule {
    pattern: Pattern,
    attrs: Vec<(BString, Attr)>,
}

/// The attribute rules that apply below one path, lowest priority first.
pub struct AttrStack {
    frames: Vec<Vec<Rule>>,
    macros: BTreeMap<BString, Vec<(BString, Attr)>>,
}

fn parse_attr_word(word: &[u8]) -> (BString, Attr) {
    if let Some(n) = word.strip_prefix(b"-") {
        (BString::from(n), Attr::Unset)
    } else if let Some(n) = word.strip_prefix(b"!") {
        (BString::from(n), Attr::Unspecified)
    } else if let Some(i) = word.iter().position(|b| *b == b'=') {
        (
            BString::from(&word[..i]),
            Attr::Value(BString::from(&word[i + 1..])),
        )
    } else {
        (BString::from(word), Attr::Set)
    }
}

fn parse_attribute_file(
    data: &[u8],
    base: &[u8],
    top: bool,
    macros: &mut BTreeMap<BString, Vec<(BString, Attr)>>,
) -> Vec<Rule> {
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
        let attrs: Vec<(BString, Attr)> = rest.fields().map(parse_attr_word).collect();
        if let Some(name) = pattern.strip_prefix(b"[attr]") {
            if top {
                macros.insert(BString::from(name), attrs);
            }
            continue;
        }
        if pattern.starts_with(b"!") {
            continue;
        }
        if let Ok(Some(pattern)) = Pattern::parse(
            BStr::new(pattern),
            IgnoreSource::CommandLine,
            0,
            BStr::new(base),
        ) {
            rules.push(Rule { pattern, attrs });
        }
    }
    rules
}

impl AttrStack {
    /// Load the attribute sources that apply to `path` (a work-tree-relative, `/`-separated path):
    /// `core.attributesFile`, the `.gitattributes` of the root and of each directory above the
    /// path, then `$GIT_DIR/info/attributes`.
    pub fn load(
        root: &Path,
        common_dir: Option<&Path>,
        config: &ConvertConfig,
        path: &BStr,
    ) -> Self {
        let mut macros = BTreeMap::new();
        let mut frames = Vec::new();
        if let Some(file) = &config.attributes_file
            && let Ok(data) = std::fs::read(file)
        {
            frames.push(parse_attribute_file(&data, b"", true, &mut macros));
        }
        let mut dir = root.to_path_buf();
        let mut base: Vec<u8> = Vec::new();
        let mut parts: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
        parts.pop();
        let mut level = 0;
        loop {
            if let Ok(data) = std::fs::read(dir.join(".gitattributes")) {
                frames.push(parse_attribute_file(&data, &base, level == 0, &mut macros));
            }
            let Some(part) = parts.get(level) else { break };
            if part.is_empty() {
                level += 1;
                continue;
            }
            dir.push(OsStr::from_bytes(part));
            if !base.is_empty() {
                base.push(b'/');
            }
            base.extend_from_slice(part);
            level += 1;
        }
        if let Some(common) = common_dir
            && let Ok(data) = std::fs::read(common.join("info/attributes"))
        {
            frames.push(parse_attribute_file(&data, b"", true, &mut macros));
        }
        Self { frames, macros }
    }

    fn apply(&self, target: &[u8], name: &[u8], state: &Attr, result: &mut Attr, depth: usize) {
        if name == target {
            *result = state.clone();
        }
        if *state != Attr::Set || depth > 8 {
            return;
        }
        if name == b"binary" {
            for n in [&b"diff"[..], b"merge", b"text"] {
                self.apply(target, n, &Attr::Unset, result, depth + 1);
            }
        } else if let Some(expansion) = self.macros.get(name.as_bstr()) {
            for (n, s) in expansion {
                self.apply(target, n, s, result, depth + 1);
            }
        }
    }

    /// The state of attribute `name` for `path`.
    pub fn lookup(&self, path: &BStr, name: &[u8]) -> Attr {
        let mut result = Attr::Unspecified;
        for frame in &self.frames {
            for rule in frame {
                if rule.pattern.matches(path, PathKind::File) {
                    for (n, s) in &rule.attrs {
                        self.apply(name, n, s, &mut result, 0);
                    }
                }
            }
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Undefined,
    Binary,
    Text,
    TextInput,
    TextCrlf,
    Auto,
    AutoInput,
    AutoCrlf,
}

impl Action {
    fn is_auto(self) -> bool {
        matches!(self, Action::Auto | Action::AutoInput | Action::AutoCrlf)
    }
}

fn crlf_attr_action(attr: &Attr) -> Action {
    match attr {
        Attr::Set => Action::Text,
        Attr::Unset => Action::Binary,
        Attr::Value(v) if v == "input" => Action::TextInput,
        Attr::Value(v) if v == "auto" => Action::Auto,
        _ => Action::Undefined,
    }
}

#[derive(Default)]
struct Stats {
    nul: usize,
    lonecr: usize,
    lonelf: usize,
    crlf: usize,
    printable: usize,
    nonprintable: isize,
}

fn gather_stats(buf: &[u8]) -> Stats {
    let mut s = Stats::default();
    let mut i = 0;
    while i < buf.len() {
        let c = buf[i];
        i += 1;
        if c == b'\r' {
            if i < buf.len() && buf[i] == b'\n' {
                s.crlf += 1;
                i += 1;
            } else {
                s.lonecr += 1;
            }
            continue;
        }
        if c == b'\n' {
            s.lonelf += 1;
            continue;
        }
        if c == 127 {
            s.nonprintable += 1;
        } else if c < 32 {
            match c {
                8 | 9 | 27 | 12 => s.printable += 1,
                0 => {
                    s.nul += 1;
                    s.nonprintable += 1;
                }
                _ => s.nonprintable += 1,
            }
        } else {
            s.printable += 1;
        }
    }
    if buf.last() == Some(&0x1a) {
        s.nonprintable -= 1;
    }
    s
}

fn is_binary(s: &Stats) -> bool {
    s.lonecr > 0 || s.nul > 0 || ((s.printable >> 7) as isize) < s.nonprintable
}

/// The per-path conversion decision.
struct Plan {
    action: Action,
}

fn refuse(path: &BStr, what: &str) -> Error {
    Error::Message(format!(
        "{}: {} is not supported by rivet; refusing to write a blob git would not have written",
        path.to_str_lossy(),
        what
    ))
}

impl ConvertConfig {
    fn plan(&self, stack: &AttrStack, path: &BStr, smudge: bool) -> Result<Plan> {
        if stack.lookup(path, b"ident") == Attr::Set {
            return Err(refuse(path, "the `ident` attribute"));
        }
        if let Attr::Value(enc) = stack.lookup(path, b"working-tree-encoding") {
            let e = enc.to_ascii_lowercase();
            if e != b"utf-8" && e != b"utf8" {
                return Err(refuse(path, "the `working-tree-encoding` attribute"));
            }
        }
        if let Attr::Value(name) = stack.lookup(path, b"filter") {
            let set = if smudge {
                &self.smudge_filters
            } else {
                &self.clean_filters
            };
            if set.contains(&name) {
                return Err(refuse(path, &format!("the `filter={}` driver", name)));
            }
        }
        let mut action = crlf_attr_action(&stack.lookup(path, b"text"));
        if action == Action::Undefined {
            action = crlf_attr_action(&stack.lookup(path, b"crlf"));
        }
        if action != Action::Binary {
            let eol = stack.lookup(path, b"eol");
            let lf = eol == Attr::Value(BString::from("lf"));
            let crlf = eol == Attr::Value(BString::from("crlf"));
            action = match (action, lf, crlf) {
                (Action::Auto, true, _) => Action::AutoInput,
                (Action::Auto, _, true) => Action::AutoCrlf,
                (_, true, _) => Action::TextInput,
                (_, _, true) => Action::TextCrlf,
                (a, _, _) => a,
            };
        }
        if action == Action::Undefined {
            action = match self.autocrlf {
                AutoCrlf::True => Action::AutoCrlf,
                AutoCrlf::Input => Action::AutoInput,
                AutoCrlf::False => Action::Undefined,
            };
        }
        Ok(Plan { action })
    }

    fn output_is_crlf(&self, action: Action) -> bool {
        match action {
            Action::Binary | Action::Undefined | Action::TextInput | Action::AutoInput => false,
            Action::TextCrlf | Action::AutoCrlf => true,
            Action::Text | Action::Auto => match self.autocrlf {
                AutoCrlf::True => true,
                AutoCrlf::Input => false,
                AutoCrlf::False => self.eol_crlf,
            },
        }
    }

    fn will_lf_to_crlf(&self, s: &Stats, action: Action) -> bool {
        if !self.output_is_crlf(action) || s.lonelf == 0 {
            return false;
        }
        if action.is_auto() && (s.lonecr > 0 || s.crlf > 0 || is_binary(s)) {
            return false;
        }
        true
    }

    /// The clean conversion (work tree → blob) of `data`, the content of `path`.
    ///
    /// `index_has_cr` is asked, only when it can matter, whether the blob the index holds for
    /// this path contains a CR. `report` turns on the `core.safecrlf` warning or refusal, which
    /// git only does when it writes the object.
    ///
    /// # Errors
    ///
    /// [`Error::Message`] for an attribute that cannot be honoured, and for `core.safecrlf=true`
    /// when the conversion would not round-trip.
    pub fn clean(
        &self,
        stack: &AttrStack,
        path: &BStr,
        data: Vec<u8>,
        index_has_cr: &mut dyn FnMut() -> bool,
        report: bool,
    ) -> Result<Vec<u8>> {
        let Plan { action } = self.plan(stack, path, false)?;
        if action == Action::Binary || action == Action::Undefined || data.is_empty() {
            return Ok(data);
        }
        let stats = gather_stats(&data);
        let mut crlf_to_lf = stats.crlf > 0;
        if action.is_auto() {
            if is_binary(&stats) {
                return Ok(data);
            }
            if crlf_to_lf && index_has_cr() {
                crlf_to_lf = false;
            }
        }
        if report && self.safecrlf != SafeCrlf::Off {
            let mut new = Stats {
                nul: stats.nul,
                lonecr: stats.lonecr,
                lonelf: stats.lonelf,
                crlf: stats.crlf,
                printable: stats.printable,
                nonprintable: stats.nonprintable,
            };
            if crlf_to_lf {
                new.lonelf += new.crlf;
                new.crlf = 0;
            }
            if self.will_lf_to_crlf(&new, action) {
                new.crlf += new.lonelf;
                new.lonelf = 0;
            }
            let shown = path.to_str_lossy();
            let problem = if stats.crlf > 0 && new.crlf == 0 {
                Some(("CRLF", "LF"))
            } else if stats.lonelf > 0 && new.lonelf == 0 {
                Some(("LF", "CRLF"))
            } else {
                None
            };
            if let Some((from, to)) = problem {
                if self.safecrlf == SafeCrlf::Die {
                    return Err(Error::Message(format!(
                        "{from} would be replaced by {to} in {shown}"
                    )));
                }
                eprintln!(
                    "warning: in the working copy of '{shown}', {from} will be replaced by {to} the next time Git touches it"
                );
            }
        }
        if !crlf_to_lf {
            return Ok(data);
        }
        let mut out = Vec::with_capacity(data.len());
        for (i, &c) in data.iter().enumerate() {
            if c == b'\r' && data.get(i + 1) == Some(&b'\n') {
                continue;
            }
            out.push(c);
        }
        Ok(out)
    }

    /// The smudge conversion (blob → work tree) of `data`, the blob content for `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Message`] for an attribute that cannot be honoured.
    pub fn smudge(&self, stack: &AttrStack, path: &BStr, data: Vec<u8>) -> Result<Vec<u8>> {
        let Plan { action } = self.plan(stack, path, true)?;
        if data.is_empty() || !self.output_is_crlf(action) {
            return Ok(data);
        }
        let stats = gather_stats(&data);
        if !self.will_lf_to_crlf(&stats, action) {
            return Ok(data);
        }
        let mut out = Vec::with_capacity(data.len() + stats.lonelf);
        for (i, &c) in data.iter().enumerate() {
            if c == b'\n' && (i == 0 || data[i - 1] != b'\r') {
                out.push(b'\r');
            }
            out.push(c);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(autocrlf: AutoCrlf) -> ConvertConfig {
        ConvertConfig {
            autocrlf,
            eol_crlf: false,
            safecrlf: SafeCrlf::Off,
            attributes_file: None,
            clean_filters: BTreeSet::new(),
            smudge_filters: BTreeSet::new(),
        }
    }

    fn stack(text: &str) -> AttrStack {
        let mut macros = BTreeMap::new();
        let frame = parse_attribute_file(text.as_bytes(), b"", true, &mut macros);
        AttrStack {
            frames: vec![frame],
            macros,
        }
    }

    fn clean(c: &ConvertConfig, s: &AttrStack, data: &[u8], idx_cr: bool) -> Vec<u8> {
        c.clean(s, BStr::new("f.txt"), data.to_vec(), &mut || idx_cr, false)
            .unwrap()
    }

    #[test]
    fn text_auto_normalises_crlf_but_not_binary_or_index_cr() {
        let c = cfg(AutoCrlf::False);
        let s = stack("* text=auto\n");
        assert_eq!(clean(&c, &s, b"a\r\nb\r\n", false), b"a\nb\n");
        assert_eq!(clean(&c, &s, b"a\0\r\nb\r\n", false), b"a\0\r\nb\r\n");
        assert_eq!(clean(&c, &s, b"a\r\nb\r\n", true), b"a\r\nb\r\n");
        assert_eq!(clean(&c, &s, b"a\rb\r\n", false), b"a\rb\r\n");
    }

    #[test]
    fn binary_macro_and_unset_text_leave_bytes_alone() {
        let c = cfg(AutoCrlf::True);
        let s = stack("* text=auto\n*.txt binary\n");
        assert_eq!(clean(&c, &s, b"a\r\n", false), b"a\r\n");
        let s = stack("* -text\n");
        assert_eq!(clean(&c, &s, b"a\r\n", false), b"a\r\n");
    }

    #[test]
    fn smudge_follows_eol_attribute_and_autocrlf() {
        let p = BStr::new("f.txt");
        let c = cfg(AutoCrlf::False);
        let s = stack("* text=auto eol=crlf\n");
        assert_eq!(c.smudge(&s, p, b"a\nb\n".to_vec()).unwrap(), b"a\r\nb\r\n");
        // a mixed file is left alone by the auto variants
        assert_eq!(c.smudge(&s, p, b"a\r\nb\n".to_vec()).unwrap(), b"a\r\nb\n");
        let s = stack("* text eol=crlf\n");
        assert_eq!(
            c.smudge(&s, p, b"a\r\nb\n".to_vec()).unwrap(),
            b"a\r\nb\r\n"
        );
        let s = stack("");
        assert_eq!(c.smudge(&s, p, b"a\n".to_vec()).unwrap(), b"a\n");
        let c = cfg(AutoCrlf::True);
        assert_eq!(c.smudge(&s, p, b"a\n".to_vec()).unwrap(), b"a\r\n");
        let c = cfg(AutoCrlf::Input);
        assert_eq!(c.smudge(&s, p, b"a\n".to_vec()).unwrap(), b"a\n");
    }

    #[test]
    fn later_lines_and_deeper_rules_win() {
        let s = stack("* text=auto\n*.txt -text\n");
        assert_eq!(s.lookup(BStr::new("a.txt"), b"text"), Attr::Unset);
        assert_eq!(
            s.lookup(BStr::new("a.c"), b"text"),
            Attr::Value("auto".into())
        );
        assert_eq!(s.lookup(BStr::new("a.c"), b"eol"), Attr::Unspecified);
    }

    #[test]
    fn ident_and_configured_filters_are_refused() {
        let c = cfg(AutoCrlf::False);
        let err = c
            .clean(
                &stack("* ident\n"),
                BStr::new("f"),
                b"x".to_vec(),
                &mut || false,
                false,
            )
            .unwrap_err();
        assert!(err.to_string().contains("ident"));
        let mut c = cfg(AutoCrlf::False);
        c.clean_filters.insert("lfs".into());
        assert!(
            c.clean(
                &stack("* filter=lfs\n"),
                BStr::new("f"),
                b"x".to_vec(),
                &mut || false,
                false
            )
            .is_err()
        );
        // a driver name nobody configured is a no-op, as in git
        assert!(
            c.clean(
                &stack("* filter=other\n"),
                BStr::new("f"),
                b"x".to_vec(),
                &mut || false,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn safecrlf_die_refuses_a_conversion_that_does_not_round_trip() {
        let mut c = cfg(AutoCrlf::False);
        c.safecrlf = SafeCrlf::Die;
        let s = stack("* text=auto\n");
        let err = c
            .clean(
                &s,
                BStr::new("n.dat"),
                b"a\r\n".to_vec(),
                &mut || false,
                true,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "CRLF would be replaced by LF in n.dat");
    }
}
