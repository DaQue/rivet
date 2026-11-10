//! Whitespace rules for `apply --whitespace=<action>`: git's `core.whitespace` / `whitespace`
//! attribute rule language, the per-line check (`ws_check`) and the line rewrite (`ws_fix_copy`),
//! and the pass over a parsed patch that reports and repairs.

use std::io::Write as _;

use bstr::{BStr, ByteSlice as _};
use rivet_core::Repository;
use rivet_core::convert::Attr;

use super::patch::{FilePatch, Hunk};

/// Trailing whitespace on a line (`blank-at-eol`).
pub const BLANK_AT_EOL: u32 = 1;
/// A space in the indent directly before a tab (`space-before-tab`).
pub const SPACE_BEFORE_TAB: u32 = 1 << 1;
/// A run of `tabwidth` spaces in the indent (`indent-with-non-tab`).
pub const INDENT_WITH_NON_TAB: u32 = 1 << 2;
/// A trailing carriage return is part of the line ending (`cr-at-eol`).
pub const CR_AT_EOL: u32 = 1 << 3;
/// A tab in the indent (`tab-in-indent`).
pub const TAB_IN_INDENT: u32 = 1 << 4;
/// Blank lines added at the end of the file (`blank-at-eof`).
pub const BLANK_AT_EOF: u32 = 1 << 5;

/// The rules in force for one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The `BLANK_AT_EOL` ... `BLANK_AT_EOF` bits that are on.
    pub flags: u32,
    /// Width of a tab, for `indent-with-non-tab` and `tab-in-indent`.
    pub tabwidth: usize,
}

impl Rule {
    /// git's default: `blank-at-eol`, `space-before-tab`, `blank-at-eof`, tabs 8 wide.
    pub const DEFAULT: Rule = Rule {
        flags: BLANK_AT_EOL | SPACE_BEFORE_TAB | BLANK_AT_EOF,
        tabwidth: 8,
    };

    /// Start from `self` and apply a comma-separated rule list (`core.whitespace` syntax):
    /// `-` negates, `trailing-space` is `blank-at-eol` plus `blank-at-eof`, `tabwidth=<1..63>`,
    /// unknown words are ignored.
    pub fn parse(mut self, list: &[u8]) -> Rule {
        for word in list.split(|b| *b == b',') {
            let word = String::from_utf8_lossy(word);
            let word = word.trim();
            let (negate, name) = match word.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, word),
            };
            let bits = match name {
                "trailing-space" => BLANK_AT_EOL | BLANK_AT_EOF,
                "blank-at-eol" => BLANK_AT_EOL,
                "blank-at-eof" => BLANK_AT_EOF,
                "space-before-tab" => SPACE_BEFORE_TAB,
                "indent-with-non-tab" => INDENT_WITH_NON_TAB,
                "cr-at-eol" => CR_AT_EOL,
                "tab-in-indent" => TAB_IN_INDENT,
                _ => {
                    if let Some(width) = name.strip_prefix("tabwidth=")
                        && let Ok(width) = width.parse::<usize>()
                        && (1..64).contains(&width)
                    {
                        self.tabwidth = width;
                    }
                    continue;
                }
            };
            if negate {
                self.flags &= !bits;
            } else {
                self.flags |= bits;
            }
        }
        self
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Whether a line (without its newline) is empty or only whitespace.
pub fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|b| is_space(*b))
}

/// The problems of one added line (without its newline): `BLANK_AT_EOL`, `SPACE_BEFORE_TAB`,
/// `INDENT_WITH_NON_TAB` and `TAB_IN_INDENT` bits.
pub fn check(line: &[u8], rule: Rule) -> u32 {
    let mut len = line.len();
    if rule.flags & CR_AT_EOL != 0 && len > 0 && line[len - 1] == b'\r' {
        len -= 1;
    }
    let mut result = 0;
    // The indent is looked at only up to the trailing whitespace.
    let mut scan = len;
    if rule.flags & BLANK_AT_EOL != 0 && len > 0 && is_space(line[len - 1]) {
        result |= BLANK_AT_EOL;
        while scan > 0 && is_space(line[scan - 1]) {
            scan -= 1;
        }
    }
    let mut spaces = 0;
    for byte in &line[..scan] {
        match byte {
            b' ' => {
                spaces += 1;
                if rule.flags & INDENT_WITH_NON_TAB != 0 && spaces >= rule.tabwidth {
                    result |= INDENT_WITH_NON_TAB;
                }
            }
            b'\t' => {
                if rule.flags & SPACE_BEFORE_TAB != 0 && spaces > 0 {
                    result |= SPACE_BEFORE_TAB;
                } else if rule.flags & TAB_IN_INDENT != 0 {
                    result |= TAB_IN_INDENT;
                }
                spaces = 0;
            }
            _ => break,
        }
    }
    result
}

/// git's wording of a `check` result.
pub fn reason(result: u32) -> String {
    let mut parts = Vec::new();
    if result & BLANK_AT_EOL != 0 {
        parts.push("trailing whitespace");
    }
    if result & SPACE_BEFORE_TAB != 0 {
        parts.push("space before tab in indent");
    }
    if result & INDENT_WITH_NON_TAB != 0 {
        parts.push("indent with spaces");
    }
    if result & TAB_IN_INDENT != 0 {
        parts.push("tab in indent");
    }
    parts.join(", ")
}

fn width(text: &[u8], tabwidth: usize) -> usize {
    text.iter().fold(0, |col, byte| {
        if *byte == b'\t' {
            col + tabwidth - col % tabwidth
        } else {
            col + 1
        }
    })
}

/// Whether git counts the repaired line among those "applied after fixing whitespace errors":
/// not when the only thing it took off was a lone carriage return.
fn counts_as_fixed(line: &[u8], result: u32, rule: Rule) -> bool {
    if result & !BLANK_AT_EOL != 0 {
        return true;
    }
    let mut len = line.len();
    if rule.flags & CR_AT_EOL != 0 && line.last() == Some(&b'\r') {
        len -= 1;
    }
    let tail = line[..len]
        .iter()
        .rev()
        .take_while(|b| is_space(**b))
        .count();
    !(tail == 1 && line[len - 1] == b'\r')
}

/// The line with its whitespace errors repaired.
pub fn fix(line: &[u8], rule: Rule) -> Vec<u8> {
    let mut len = line.len();
    let cr = rule.flags & CR_AT_EOL != 0 && len > 0 && line[len - 1] == b'\r';
    if cr {
        len -= 1;
    }
    if rule.flags & BLANK_AT_EOL != 0 {
        while len > 0 && is_space(line[len - 1]) {
            len -= 1;
        }
    }
    let mut out: Vec<u8> = Vec::with_capacity(len + 8);
    // The indent is rewritten only when it has a problem; then every run of `tabwidth` spaces
    // becomes a tab, whichever rule found the problem.
    let problem = check(
        &line[..len],
        Rule {
            flags: rule.flags & !BLANK_AT_EOL,
            ..rule
        },
    );
    if problem & (SPACE_BEFORE_TAB | INDENT_WITH_NON_TAB | TAB_IN_INDENT) == 0 {
        out.extend_from_slice(&line[..len]);
        if cr {
            out.push(b'\r');
        }
        return out;
    }
    let mut spaces = 0;
    let mut at = 0;
    while at < len {
        match line[at] {
            b' ' => {
                out.push(b' ');
                spaces += 1;
                if rule.flags & TAB_IN_INDENT == 0 && spaces == rule.tabwidth {
                    out.truncate(out.len() - spaces);
                    out.push(b'\t');
                    spaces = 0;
                }
            }
            b'\t' => {
                if rule.flags & SPACE_BEFORE_TAB != 0 && spaces > 0 {
                    out.truncate(out.len() - spaces);
                    out.push(b'\t');
                } else if rule.flags & TAB_IN_INDENT != 0 {
                    let col = width(&out, rule.tabwidth);
                    out.resize(out.len() + rule.tabwidth - col % rule.tabwidth, b' ');
                } else {
                    out.push(b'\t');
                }
                spaces = 0;
            }
            _ => break,
        }
        at += 1;
    }
    out.extend_from_slice(&line[at..len]);
    if cr {
        out.push(b'\r');
    }
    out
}

/// What `--whitespace=<action>` / `apply.whitespace` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Say nothing.
    NoWarn,
    /// Report, apply anyway.
    Warn,
    /// Report every problem line (up to five), then refuse the patch.
    Error,
    /// Like `Error`, but report every line.
    ErrorAll,
    /// Repair the added lines.
    Fix,
}

impl Action {
    /// The action a word names (`strip` is `fix`).
    pub fn parse(word: &str) -> Option<Action> {
        match word {
            "nowarn" => Some(Action::NoWarn),
            "warn" => Some(Action::Warn),
            "error" => Some(Action::Error),
            "error-all" => Some(Action::ErrorAll),
            "fix" | "strip" => Some(Action::Fix),
            _ => None,
        }
    }
}

/// Where the lines of `patch_text` came from: `(line index the input starts at, name)`.
pub type Sources = [(usize, String)];

/// The rules for one path: `core.whitespace`, then the `whitespace` attribute.
fn rule_for(repository: &Repository, path: &[u8]) -> Rule {
    let base = match repository.config().get(BStr::new("core.whitespace")) {
        Ok(Some(value)) => Rule::DEFAULT.parse(value.as_bytes()),
        _ => Rule::DEFAULT,
    };
    let attr = repository
        .worktree()
        .ok()
        .and_then(|worktree| worktree.attr_stack(BStr::new(path)).ok())
        .map(|stack| stack.lookup(BStr::new(path), b"whitespace"));
    match attr {
        Some(Attr::Set) => Rule {
            flags: base.flags
                | BLANK_AT_EOL
                | SPACE_BEFORE_TAB
                | INDENT_WITH_NON_TAB
                | BLANK_AT_EOF,
            ..base
        },
        Some(Attr::Unset) => Rule { flags: 0, ..base },
        Some(Attr::Value(value)) => Rule::DEFAULT.parse(value.as_bytes()),
        _ => base,
    }
}

/// The index, in the raw hunk text, of every entry of `hunk.lines` (`\ No newline` markers sit
/// between them).
fn offsets(hunk: &Hunk) -> Vec<usize> {
    hunk.raw
        .split(|b| *b == b'\n')
        .enumerate()
        .skip(1)
        .filter(|(_, line)| !line.starts_with(b"\\"))
        .map(|(at, _)| at)
        .take(hunk.lines.len())
        .collect()
}

/// The state between the two halves of the whitespace pass.
pub struct Pass {
    sources: Vec<(usize, String)>,
    rules: Vec<Rule>,
    action: Action,
    applies: bool,
    reverse: bool,
    unidiff_zero: bool,
    errors: usize,
    rewritten: usize,
}

impl Pass {
    fn die(&self) -> bool {
        matches!(self.action, Action::Error | Action::ErrorAll)
    }

    fn squelch(&self) -> Option<usize> {
        if self.action == Action::ErrorAll {
            None
        } else {
            Some(5)
        }
    }

    fn say(&self, at: usize, reason: &str, text: &[u8]) {
        if self.squelch().is_some_and(|limit| limit < self.errors) {
            return;
        }
        let (start, name) = self
            .sources
            .iter()
            .rev()
            .find(|(start, _)| *start <= at)
            .map_or((0, "<stdin>"), |(start, name)| (*start, name.as_str()));
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{name}:{}: {reason}.", at - start + 1);
        let _ = err.write_all(text);
        let _ = err.write_all(b"\n");
    }

    /// The second half, run once the hunks are known to fit: blank lines at the end of the file,
    /// then git's summary. Returns the exit code when the patch is refused.
    pub fn finish(mut self, patches: &mut [FilePatch]) -> Option<i32> {
        let die = self.die();
        let skip_eof =
            self.reverse || self.unidiff_zero || (die && self.applies && self.errors > 0);
        let rules = std::mem::take(&mut self.rules);
        for (patch, rule) in patches.iter_mut().zip(&rules) {
            if skip_eof || patch.binary || rule.flags & BLANK_AT_EOF == 0 {
                continue;
            }
            for hunk in &mut patch.hunks {
                let run = hunk
                    .lines
                    .iter()
                    .rev()
                    .take_while(|(kind, text)| *kind == b'+' && is_blank(text))
                    .count();
                if run == 0 {
                    continue;
                }
                let first = hunk.lines.len() - run;
                self.errors += 1;
                self.say(
                    hunk.first_line + offsets(hunk)[first],
                    "new blank line at EOF",
                    b"+",
                );
                if self.action == Action::Fix {
                    hunk.lines.truncate(first);
                }
            }
        }
        let errors = self.errors;
        if errors > 0
            && let Some(limit) = self.squelch()
            && limit < errors
        {
            let n = errors - limit;
            eprintln!(
                "warning: squelched {n} whitespace error{}",
                if n == 1 { "" } else { "s" }
            );
        }
        if die && errors > 0 {
            eprintln!(
                "error: {errors} line{} whitespace errors.",
                if errors == 1 { " adds" } else { "s add" }
            );
            return Some(128);
        }
        if self.reverse {
            return None;
        }
        if self.rewritten > 0 && self.applies {
            eprintln!(
                "warning: {} line{} applied after fixing whitespace errors.",
                self.rewritten,
                if self.rewritten == 1 { "" } else { "s" }
            );
        } else if errors > 0 {
            eprintln!(
                "warning: {errors} line{} whitespace errors.",
                if errors == 1 { " adds" } else { "s add" }
            );
        }
        None
    }
}

/// The first half: check the lines `patches` add under `action`, print git's report of them on
/// standard error and, for `Fix`, rewrite them. `applies` is false when nothing will be written
/// (`--check`, `--stat`). `None` when the action is `nowarn`.
pub fn begin(
    repository: &Repository,
    patches: &mut [FilePatch],
    sources: &Sources,
    action: Action,
    applies: bool,
    reverse: bool,
    unidiff_zero: bool,
) -> Option<Pass> {
    if action == Action::NoWarn {
        return None;
    }
    // In reverse git reports the lines the reversal adds, i.e. the ones the patch removes, and
    // neither summarises nor repairs them.
    let added = if reverse { b'-' } else { b'+' };
    let rules: Vec<Rule> = patches
        .iter()
        .map(|patch| {
            let path = patch.new_path.as_ref().or(patch.old_path.as_ref());
            path.map_or(Rule::DEFAULT, |path| rule_for(repository, path))
        })
        .collect();
    let mut state = Pass {
        sources: sources.to_vec(),
        rules: Vec::new(),
        action,
        applies,
        reverse,
        unidiff_zero,
        errors: 0,
        rewritten: 0,
    };
    for (patch, rule) in patches.iter_mut().zip(&rules) {
        if patch.binary || rule.flags == 0 {
            continue;
        }
        for hunk in &mut patch.hunks {
            let at = offsets(hunk);
            let last = hunk.lines.len();
            for (n, (kind, text)) in hunk.lines.iter_mut().enumerate() {
                if *kind != added {
                    continue;
                }
                let result = check(text, *rule);
                if result == 0 {
                    continue;
                }
                state.errors += 1;
                // git cuts the last byte off a line that ends the file without a newline.
                let shown = if hunk.new_no_newline && n + 1 == last {
                    &text[..text.len() - 1]
                } else {
                    &text[..]
                };
                state.say(hunk.first_line + at[n], &reason(result), shown);
                if action == Action::Fix && !reverse {
                    if counts_as_fixed(text, result, *rule) {
                        state.rewritten += 1;
                    }
                    *text = fix(text, *rule);
                }
            }
        }
    }
    state.rules = rules;
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_lists_follow_core_whitespace() {
        let rule = Rule::DEFAULT.parse(b"-trailing-space, indent-with-non-tab,tabwidth=4,bogus");
        assert_eq!(rule.flags, SPACE_BEFORE_TAB | INDENT_WITH_NON_TAB);
        assert_eq!(rule.tabwidth, 4);
        assert_eq!(Rule::DEFAULT.parse(b"tabwidth=64").tabwidth, 8);
    }

    #[test]
    fn check_names_each_problem() {
        let all = Rule::DEFAULT.parse(b"indent-with-non-tab");
        assert_eq!(check(b"x  ", all), BLANK_AT_EOL);
        assert_eq!(check(b" \tx", all), SPACE_BEFORE_TAB);
        assert_eq!(check(b"        x", all), INDENT_WITH_NON_TAB);
        assert_eq!(check(b"       x", all), 0);
        assert_eq!(check(b"x\r", all), BLANK_AT_EOL);
        assert_eq!(check(b"x\r", all.parse(b"cr-at-eol")), 0);
        let tabs = Rule::DEFAULT.parse(b"tab-in-indent,-space-before-tab");
        assert_eq!(check(b" \tx", tabs), TAB_IN_INDENT);
        assert_eq!(
            reason(BLANK_AT_EOL | SPACE_BEFORE_TAB),
            "trailing whitespace, space before tab in indent"
        );
    }

    #[test]
    fn fix_rewrites_the_indent_and_the_tail() {
        let rule = Rule::DEFAULT.parse(b"indent-with-non-tab");
        assert_eq!(fix(b"         x \t", rule), b"\t x");
        assert_eq!(fix(b"    \t    x", rule), b"\t    x");
        assert_eq!(fix(b"\t\t  \tx", rule), b"\t\t\tx");
        // A tab after spaces is a space-before-tab problem first: the spaces go, the tab stays.
        let both = Rule::DEFAULT.parse(b"tab-in-indent");
        assert_eq!(fix(b"  \tx", both), b"\tx");
        let tabs = Rule::DEFAULT.parse(b"tab-in-indent,-space-before-tab");
        assert_eq!(fix(b"  \tx", tabs), b"        x");
        assert_eq!(fix(b"        x", Rule::DEFAULT), b"        x");
        assert_eq!(fix(b"x \r", Rule::DEFAULT.parse(b"cr-at-eol")), b"x\r");
    }
}
