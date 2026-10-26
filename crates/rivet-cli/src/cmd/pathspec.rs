//! Git's pathspec magic for commands that filter a finished result list (`status`).
//!
//! Supported: `:(top)`, `:(literal)`, `:(icase)`, `:(glob)`, `:(exclude)` and the short forms
//! `:/`, `:!`, `:^`. Without `glob`, a pattern with wildcards is `wildmatch` with no flags, so `*`
//! crosses `/`; with `glob` it is `WM_PATHNAME`. `:(attr:...)` is refused by name.

/// One parsed pathspec element; the pattern is relative to the work-tree root.
struct Spec {
    exclude: bool,
    literal: bool,
    glob: bool,
    icase: bool,
    pattern: Vec<u8>,
}

/// A list of pathspecs: positive ones select, `exclude` ones remove.
pub struct Pathspecs {
    specs: Vec<Spec>,
}

/// Does this command-line word need the magic parser (magic prefix or a wildcard)?
pub fn needs_parser(word: &str) -> bool {
    word.starts_with(':') || word.contains(['*', '?', '['])
}

const MAGIC_PUNCT: &str = "!\"#%&',-/;<=>@_`~^";

impl Pathspecs {
    /// Parse `words`, anchoring non-`top` patterns at `prefix` (the current directory relative to
    /// the work-tree root, no trailing slash; `root` is only used in error text). The error is git's `fatal:` text.
    pub fn parse(words: &[String], prefix: &[u8], root: &str) -> Result<Pathspecs, String> {
        let mut specs = Vec::new();
        for word in words {
            let mut spec = Spec {
                exclude: false,
                literal: false,
                glob: false,
                icase: false,
                pattern: Vec::new(),
            };
            let mut top = false;
            let mut rest = word.as_str();
            if let Some(after) = word.strip_prefix(":(") {
                let Some((body, pattern)) = after.split_once(')') else {
                    return Err(format!(
                        "Missing ')' at the end of pathspec magic in '{word}'"
                    ));
                };
                for name in body.split(',').filter(|n| !n.is_empty()) {
                    match name {
                        "top" => top = true,
                        "literal" => spec.literal = true,
                        "icase" => spec.icase = true,
                        "glob" => spec.glob = true,
                        "exclude" => spec.exclude = true,
                        n if n == "attr" || n.starts_with("attr:") => {
                            return Err(format!(
                                "pathspec magic 'attr' in '{word}' is not supported by rivet"
                            ));
                        }
                        n => {
                            return Err(format!("Invalid pathspec magic '{n}' in '{word}'"));
                        }
                    }
                }
                rest = pattern;
            } else if let Some(after) = word.strip_prefix(':') {
                let mut chars = after.char_indices();
                let mut consumed = 0;
                for (i, c) in chars.by_ref() {
                    match c {
                        '/' => top = true,
                        '!' | '^' => spec.exclude = true,
                        c if MAGIC_PUNCT.contains(c) => {
                            return Err(format!("Unimplemented pathspec magic '{c}' in '{word}'"));
                        }
                        _ => break,
                    }
                    consumed = i + c.len_utf8();
                }
                rest = &after[consumed..];
                if let Some(stripped) = rest.strip_prefix(':') {
                    rest = stripped;
                }
            }
            if spec.glob && spec.literal {
                return Err(format!("{word}: 'literal' and 'glob' are incompatible"));
            }
            let joined: Vec<u8> = if top || prefix.is_empty() {
                rest.as_bytes().to_vec()
            } else {
                [prefix, b"/", rest.as_bytes()].concat()
            };
            spec.pattern = normalize(&joined)
                .ok_or_else(|| format!("{word}: '{rest}' is outside repository at '{root}'"))?;
            specs.push(spec);
        }
        Ok(Pathspecs { specs })
    }

    /// Does the file at `path` pass the include and exclude lists?
    pub fn matches_file(&self, path: &[u8]) -> bool {
        self.selects(|spec| spec.matches(path, false))
    }

    /// Does the whole directory `dir` (no trailing slash) pass, so that it may be shown collapsed?
    pub fn matches_dir(&self, dir: &[u8]) -> bool {
        self.selects(|spec| spec.matches(dir, true))
    }

    fn selects(&self, hit: impl Fn(&Spec) -> bool) -> bool {
        let mut positive = false;
        let mut included = false;
        for spec in &self.specs {
            if spec.exclude {
                if hit(spec) {
                    return false;
                }
            } else {
                positive = true;
                included |= hit(spec);
            }
        }
        included || !positive
    }
}

/// Resolve `.` and `..` components lexically; `None` when the path climbs above the root.
fn normalize(path: &[u8]) -> Option<Vec<u8>> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join(&b'/');
    if path.ends_with(b"/") && !out.is_empty() {
        out.push(b'/');
    }
    Some(out)
}

impl Spec {
    fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        let fold = |bytes: &[u8]| -> Vec<u8> {
            if self.icase {
                bytes.to_ascii_lowercase()
            } else {
                bytes.to_vec()
            }
        };
        let pattern = fold(&self.pattern);
        let path = fold(path);
        let bare = pattern.strip_suffix(b"/").unwrap_or(&pattern);
        if bare.is_empty() {
            return true;
        }
        if path == bare || (path.starts_with(bare) && path.get(bare.len()) == Some(&b'/')) {
            return true;
        }
        if self.literal
            || !pattern
                .iter()
                .any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
        {
            return false;
        }
        let mut subject = path;
        if is_dir {
            subject.push(b'/');
        }
        if self.glob {
            super::query::glob_match(&pattern, &subject)
        } else {
            super::branching::glob_match(&pattern, &subject)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str], prefix: &str) -> Result<Pathspecs, String> {
        let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        Pathspecs::parse(&words, prefix.as_bytes(), "/r")
    }

    #[test]
    fn star_crosses_slash_only_without_glob_magic() {
        let plain = parse(&["*.c"], "").unwrap();
        assert!(plain.matches_file(b"d/x.c"));
        let glob = parse(&[":(glob)*.c"], "").unwrap();
        assert!(!glob.matches_file(b"d/x.c"));
        assert!(glob.matches_file(b"x.c"));
    }

    #[test]
    fn exclude_only_selects_everything_else() {
        let spec = parse(&[":!d"], "").unwrap();
        assert!(spec.matches_file(b"a.txt"));
        assert!(!spec.matches_file(b"d/b.c"));
    }

    #[test]
    fn top_ignores_the_prefix_and_dotdot_climbs() {
        assert!(parse(&[":/a.txt"], "d").unwrap().matches_file(b"a.txt"));
        assert!(parse(&["../*.c"], "d").unwrap().matches_file(b"u/x.c"));
        assert!(parse(&["../../x"], "d").is_err());
    }

    #[test]
    fn directories_collapse_only_when_the_whole_directory_matches() {
        assert!(parse(&["u/*"], "").unwrap().matches_dir(b"u"));
        assert!(!parse(&["*.c"], "").unwrap().matches_dir(b"u"));
        assert!(!parse(&[":!u"], "").unwrap().matches_dir(b"u"));
    }

    #[test]
    fn bad_magic_gets_gits_text() {
        let err = |w: &str| parse(&[w], "").err().unwrap();
        assert_eq!(err(":(foo)x"), "Invalid pathspec magic 'foo' in ':(foo)x'");
        assert_eq!(
            err(":(top"),
            "Missing ')' at the end of pathspec magic in ':(top'"
        );
        assert_eq!(
            err(":(glob,literal)x"),
            ":(glob,literal)x: 'literal' and 'glob' are incompatible"
        );
        assert_eq!(err(":#x"), "Unimplemented pathspec magic '#' in ':#x'");
        assert!(err(":(attr:a)x").contains("not supported by rivet"));
    }

    #[test]
    fn icase_and_literal() {
        assert!(
            parse(&[":(icase)A.TXT"], "")
                .unwrap()
                .matches_file(b"a.txt")
        );
        assert!(!parse(&[":(literal)*.c"], "").unwrap().matches_file(b"x.c"));
        assert!(parse(&[":(literal)*.c"], "").unwrap().matches_file(b"*.c"));
    }
}
