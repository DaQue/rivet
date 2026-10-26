//! A small POSIX regular-expression matcher for `grep`: basic (BRE, with the GNU `\+ \? \|`
//! extensions) and extended (ERE) syntax over bytes, existence of a match and the set of match
//! ends at a position (enough for `-w`).
//!
//! # Caveats
//!
//! * **Matching is by backtracking**, so a pathological pattern can be slow. Patterns come from the
//!   command line, not from repository data.
//! * **Back-references and `-o` leftmost-longest reporting are not supported**; the former is a
//!   parse error here, the latter is refused by `grep`.
//! * Repetition counts are expanded by copying the sub-pattern, capped at 1000 copies.

use anyhow::{Result, bail};

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Byte(u8),
    Any,
    Class(Box<[bool; 256]>),
    LineStart,
    LineEnd,
    WordBoundary,
    NotWordBoundary,
    WordStart,
    WordEnd,
    Group(Box<Node>),
    Alt(Vec<Node>),
    Concat(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>),
}

#[derive(Clone, Debug)]
enum Inst {
    Byte(u8),
    Any,
    Class(Box<[bool; 256]>),
    LineStart,
    LineEnd,
    WordBoundary,
    NotWordBoundary,
    WordStart,
    WordEnd,
    Split(usize, usize),
    Jmp(usize),
    Match,
}

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Regex {
    program: Vec<Inst>,
}

struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
    extended: bool,
    ignore_case: bool,
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn named_class(name: &[u8]) -> Option<fn(u8) -> bool> {
    Some(match name {
        b"alpha" => |b| b.is_ascii_alphabetic(),
        b"digit" => |b| b.is_ascii_digit(),
        b"alnum" => |b| b.is_ascii_alphanumeric(),
        b"upper" => |b| b.is_ascii_uppercase(),
        b"lower" => |b| b.is_ascii_lowercase(),
        b"space" => |b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b"blank" => |b| b == b' ' || b == b'\t',
        b"punct" => |b| b.is_ascii_punctuation(),
        b"print" => |b| (0x20..0x7f).contains(&b),
        b"graph" => |b| (0x21..0x7f).contains(&b),
        b"cntrl" => |b| b < 0x20 || b == 0x7f,
        b"xdigit" => |b| b.is_ascii_hexdigit(),
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<(u8, u8)> {
        Some((*self.text.get(self.pos)?, *self.text.get(self.pos + 1)?))
    }

    fn at_alt(&self) -> bool {
        if self.extended {
            self.peek() == Some(b'|')
        } else {
            self.peek2() == Some((b'\\', b'|'))
        }
    }

    fn at_close(&self) -> bool {
        if self.extended {
            self.peek() == Some(b')')
        } else {
            self.peek2() == Some((b'\\', b')'))
        }
    }

    fn literal(&self, byte: u8) -> Node {
        if self.ignore_case && byte.is_ascii_alphabetic() {
            let mut set = Box::new([false; 256]);
            set[byte.to_ascii_lowercase() as usize] = true;
            set[byte.to_ascii_uppercase() as usize] = true;
            Node::Class(set)
        } else {
            Node::Byte(byte)
        }
    }

    fn parse_alt(&mut self, depth: usize) -> Result<Node> {
        let mut branches = vec![self.parse_concat(depth)?];
        while self.at_alt() {
            self.pos += if self.extended { 1 } else { 2 };
            branches.push(self.parse_concat(depth)?);
        }
        Ok(if branches.len() == 1 {
            branches.remove(0)
        } else {
            Node::Alt(branches)
        })
    }

    fn parse_concat(&mut self, depth: usize) -> Result<Node> {
        let mut items: Vec<Node> = Vec::new();
        let mut first = true;
        while self.pos < self.text.len() && !self.at_alt() {
            if depth > 0 && self.at_close() {
                break;
            }
            if depth == 0 && self.at_close() {
                bail!("Unmatched ) or \\)");
            }
            let atom = self.parse_atom(first)?;
            first = false;
            let atom = self.parse_repeats(atom)?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.remove(0),
            _ => Node::Concat(items),
        })
    }

    fn parse_repeats(&mut self, mut atom: Node) -> Result<Node> {
        loop {
            let (min, max, used) = match self.peek() {
                Some(b'*') => (0, None, 1),
                Some(b'+') if self.extended => (1, None, 1),
                Some(b'?') if self.extended => (0, Some(1), 1),
                Some(b'\\') if !self.extended => match self.text.get(self.pos + 1) {
                    Some(b'+') => (1, None, 2),
                    Some(b'?') => (0, Some(1), 2),
                    Some(b'{') => match self.parse_bound(self.pos + 2, b"\\}") {
                        Some((min, max, end)) => (min, max, end - self.pos),
                        None => bail!("Invalid content of \\{{\\}}"),
                    },
                    _ => break,
                },
                Some(b'{') if self.extended => match self.parse_bound(self.pos + 1, b"}") {
                    Some((min, max, end)) => (min, max, end - self.pos),
                    None => break,
                },
                _ => break,
            };
            self.pos += used;
            if max.is_some_and(|m| m < min) {
                bail!("Invalid content of \\{{\\}}");
            }
            atom = Node::Repeat(Box::new(atom), min, max);
        }
        Ok(atom)
    }

    /// `m`, `m,`, `m,n` or `,n` followed by `close`; returns the bounds and the end position.
    fn parse_bound(&self, mut at: usize, close: &[u8]) -> Option<(usize, Option<usize>, usize)> {
        let number = |at: &mut usize| -> Option<usize> {
            let start = *at;
            while self.text.get(*at).is_some_and(u8::is_ascii_digit) {
                *at += 1;
            }
            std::str::from_utf8(&self.text[start..*at])
                .ok()?
                .parse()
                .ok()
        };
        let min = number(&mut at);
        let (min, max) = if self.text.get(at) == Some(&b',') {
            at += 1;
            let max = number(&mut at);
            (min.unwrap_or(0), max)
        } else {
            (min?, min)
        };
        if self.text[at..].starts_with(close) {
            Some((min, max, at + close.len()))
        } else {
            None
        }
    }

    fn parse_atom(&mut self, first: bool) -> Result<Node> {
        let byte = self.text[self.pos];
        self.pos += 1;
        Ok(match byte {
            b'.' => Node::Any,
            b'[' => self.parse_bracket()?,
            b'^' if self.extended || first => Node::LineStart,
            b'$' if self.extended
                || self.pos == self.text.len()
                || self.at_alt()
                || self.at_close() =>
            {
                Node::LineEnd
            }
            b'*' if first && !self.extended => self.literal(b'*'),
            b'*' | b'+' | b'?' if self.extended && first => {
                bail!("Invalid preceding regular expression")
            }
            b'(' if self.extended => {
                let inner = self.parse_alt(1)?;
                if self.peek() != Some(b')') {
                    bail!("Unmatched ( or \\(");
                }
                self.pos += 1;
                Node::Group(Box::new(inner))
            }
            b'\\' => {
                let Some(next) = self.peek() else {
                    bail!("Trailing backslash");
                };
                self.pos += 1;
                match next {
                    b'(' if !self.extended => {
                        let inner = self.parse_alt(1)?;
                        if self.peek2() != Some((b'\\', b')')) {
                            bail!("Unmatched ( or \\(");
                        }
                        self.pos += 2;
                        Node::Group(Box::new(inner))
                    }
                    b'w' | b'W' | b's' | b'S' => {
                        let mut set = Box::new([false; 256]);
                        for b in 0..=255u8 {
                            let is = match next {
                                b'w' | b'W' => is_word(b),
                                _ => matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
                            };
                            set[b as usize] = is == next.is_ascii_lowercase();
                        }
                        Node::Class(set)
                    }
                    b'b' => Node::WordBoundary,
                    b'B' => Node::NotWordBoundary,
                    b'<' => Node::WordStart,
                    b'>' => Node::WordEnd,
                    b'1'..=b'9' => bail!("Invalid back reference"),
                    other => self.literal(other),
                }
            }
            other => self.literal(other),
        })
    }

    fn parse_bracket(&mut self) -> Result<Node> {
        let mut set = Box::new([false; 256]);
        let mut negate = false;
        if self.peek() == Some(b'^') {
            negate = true;
            self.pos += 1;
        }
        let mut first = true;
        loop {
            let Some(byte) = self.peek() else {
                bail!("Unmatched [, [^, [:, [., or [=");
            };
            self.pos += 1;
            if byte == b']' && !first {
                break;
            }
            first = false;
            if byte == b'[' && self.peek() == Some(b':') {
                let rest = &self.text[self.pos + 1..];
                if let Some(end) = rest.windows(2).position(|w| w == b":]") {
                    let Some(test) = named_class(&rest[..end]) else {
                        bail!("Invalid character class name");
                    };
                    for b in 0..=255u8 {
                        if test(b) {
                            set[b as usize] = true;
                        }
                    }
                    self.pos += 1 + end + 2;
                    continue;
                }
            }
            let mut low = byte;
            if self.peek() == Some(b'-') && self.text.get(self.pos + 1).is_some_and(|b| *b != b']')
            {
                self.pos += 1;
                let high = self.text[self.pos];
                self.pos += 1;
                if high < low {
                    bail!("Invalid range end");
                }
                for b in low..=high {
                    set[b as usize] = true;
                }
                low = high;
            } else {
                set[low as usize] = true;
            }
            let _ = low;
        }
        if self.ignore_case {
            for b in b'a'..=b'z' {
                if set[b as usize] || set[b.to_ascii_uppercase() as usize] {
                    set[b as usize] = true;
                    set[b.to_ascii_uppercase() as usize] = true;
                }
            }
        }
        if negate {
            for slot in set.iter_mut() {
                *slot = !*slot;
            }
            set[b'\n' as usize] = false;
        }
        Ok(Node::Class(set))
    }
}

fn emit(node: &Node, program: &mut Vec<Inst>) -> Result<()> {
    match node {
        Node::Empty => {}
        Node::Byte(b) => program.push(Inst::Byte(*b)),
        Node::Any => program.push(Inst::Any),
        Node::Class(set) => program.push(Inst::Class(set.clone())),
        Node::LineStart => program.push(Inst::LineStart),
        Node::LineEnd => program.push(Inst::LineEnd),
        Node::WordBoundary => program.push(Inst::WordBoundary),
        Node::NotWordBoundary => program.push(Inst::NotWordBoundary),
        Node::WordStart => program.push(Inst::WordStart),
        Node::WordEnd => program.push(Inst::WordEnd),
        Node::Group(inner) => emit(inner, program)?,
        Node::Concat(items) => {
            for item in items {
                emit(item, program)?;
            }
        }
        Node::Alt(branches) => {
            let mut jumps = Vec::new();
            for (i, branch) in branches.iter().enumerate() {
                if i + 1 < branches.len() {
                    let split = program.len();
                    program.push(Inst::Split(split + 1, 0));
                    emit(branch, program)?;
                    jumps.push(program.len());
                    program.push(Inst::Jmp(0));
                    let next = program.len();
                    program[split] = Inst::Split(split + 1, next);
                } else {
                    emit(branch, program)?;
                }
            }
            let end = program.len();
            for jump in jumps {
                program[jump] = Inst::Jmp(end);
            }
        }
        Node::Repeat(inner, min, max) => {
            if *min > 1000 || max.is_some_and(|m| m > 1000) {
                bail!("Regular expression too big");
            }
            for _ in 0..*min {
                emit(inner, program)?;
            }
            match max {
                None => {
                    let split = program.len();
                    program.push(Inst::Split(split + 1, 0));
                    emit(inner, program)?;
                    program.push(Inst::Jmp(split));
                    let end = program.len();
                    program[split] = Inst::Split(split + 1, end);
                }
                Some(max) => {
                    let mut splits = Vec::new();
                    for _ in *min..*max {
                        splits.push(program.len());
                        program.push(Inst::Split(0, 0));
                        emit(inner, program)?;
                    }
                    let end = program.len();
                    for split in splits {
                        program[split] = Inst::Split(split + 1, end);
                    }
                }
            }
        }
    }
    Ok(())
}

impl Regex {
    /// Compile `pattern` as an ERE (`extended`) or BRE.
    ///
    /// # Errors
    ///
    /// A message in the style of `regcomp` for a malformed pattern.
    pub fn new(pattern: &[u8], extended: bool, ignore_case: bool) -> Result<Self> {
        let mut parser = Parser {
            text: pattern,
            pos: 0,
            extended,
            ignore_case,
        };
        let node = parser.parse_alt(0)?;
        if parser.pos < pattern.len() {
            bail!("Unmatched ) or \\)");
        }
        let mut program = Vec::new();
        emit(&node, &mut program)?;
        program.push(Inst::Match);
        Ok(Self { program })
    }

    /// A pattern that matches `text` literally.
    pub fn literal(text: &[u8], ignore_case: bool) -> Self {
        let mut program = Vec::new();
        for &byte in text {
            if ignore_case && byte.is_ascii_alphabetic() {
                let mut set = Box::new([false; 256]);
                set[byte.to_ascii_lowercase() as usize] = true;
                set[byte.to_ascii_uppercase() as usize] = true;
                program.push(Inst::Class(set));
            } else {
                program.push(Inst::Byte(byte));
            }
        }
        program.push(Inst::Match);
        Self { program }
    }

    /// Every end position of a match starting exactly at `start` in `line`.
    pub fn ends_at(&self, line: &[u8], start: usize) -> Vec<usize> {
        let mut ends = Vec::new();
        let mut seen = vec![false; self.program.len() * (line.len() + 1)];
        let mut stack = vec![(0usize, start)];
        let width = line.len() + 1;
        while let Some((pc, pos)) = stack.pop() {
            if seen[pc * width + pos] {
                continue;
            }
            seen[pc * width + pos] = true;
            let before = pos.checked_sub(1).map(|i| line[i]);
            let after = line.get(pos).copied();
            let word_before = before.is_some_and(is_word);
            let word_after = after.is_some_and(is_word);
            match &self.program[pc] {
                Inst::Match => ends.push(pos),
                Inst::Byte(b) => {
                    if after == Some(*b) {
                        stack.push((pc + 1, pos + 1));
                    }
                }
                Inst::Any => {
                    if after.is_some() {
                        stack.push((pc + 1, pos + 1));
                    }
                }
                Inst::Class(set) => {
                    if after.is_some_and(|b| set[b as usize]) {
                        stack.push((pc + 1, pos + 1));
                    }
                }
                Inst::LineStart => {
                    if pos == 0 {
                        stack.push((pc + 1, pos));
                    }
                }
                Inst::LineEnd => {
                    if pos == line.len() {
                        stack.push((pc + 1, pos));
                    }
                }
                Inst::WordBoundary => {
                    if word_before != word_after {
                        stack.push((pc + 1, pos));
                    }
                }
                Inst::NotWordBoundary => {
                    if word_before == word_after {
                        stack.push((pc + 1, pos));
                    }
                }
                Inst::WordStart => {
                    if !word_before && word_after {
                        stack.push((pc + 1, pos));
                    }
                }
                Inst::WordEnd => {
                    if word_before && !word_after {
                        stack.push((pc + 1, pos));
                    }
                }
                Inst::Split(a, b) => {
                    stack.push((*b, pos));
                    stack.push((*a, pos));
                }
                Inst::Jmp(target) => stack.push((*target, pos)),
            }
        }
        ends
    }

    /// The leftmost-longest match starting at or after `from`, as `(start, end)`.
    ///
    /// Empty matches are reported (`start == end`) unless `whole_words` is set, in which case
    /// only non-empty matches bounded by non-word characters count, as in `is_match`.
    pub fn find_from(&self, line: &[u8], from: usize, whole_words: bool) -> Option<(usize, usize)> {
        for start in from..=line.len() {
            let ends = self.ends_at(line, start);
            if !whole_words {
                if let Some(&end) = ends.iter().max() {
                    return Some((start, end));
                }
                continue;
            }
            if start.checked_sub(1).is_some_and(|i| is_word(line[i])) {
                continue;
            }
            let best = ends
                .iter()
                .copied()
                .filter(|&end| end > start && !line.get(end).copied().is_some_and(is_word))
                .max();
            if let Some(end) = best {
                return Some((start, end));
            }
        }
        None
    }

    /// Whether any match starts in `line`; with `whole_words` only matches bounded by
    /// non-word characters count.
    pub fn is_match(&self, line: &[u8], whole_words: bool) -> bool {
        for start in 0..=line.len() {
            let ends = self.ends_at(line, start);
            if ends.is_empty() {
                continue;
            }
            if !whole_words {
                return true;
            }
            let word_before = start.checked_sub(1).is_some_and(|i| is_word(line[i]));
            if word_before {
                continue;
            }
            if ends
                .iter()
                .any(|&end| end > start && !line.get(end).copied().is_some_and(is_word))
            {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, extended: bool, text: &str) -> bool {
        Regex::new(pattern.as_bytes(), extended, false)
            .expect("compiles")
            .is_match(text.as_bytes(), false)
    }

    #[test]
    fn basic_syntax() {
        assert!(m("a.c", false, "xabcx"));
        assert!(m("^ab*c$", false, "abbbc"));
        assert!(!m("^ab*c$", false, "xabc"));
        assert!(m("a\\+b", false, "caab"));
        assert!(m("a+b", false, "a+b"));
        assert!(m("\\(ab\\)\\{2\\}", false, "xababx"));
        assert!(m("foo\\|bar", false, "a bar"));
        assert!(m("*x", false, "*x"));
        assert!(m("[[:digit:]]\\{3\\}", false, "ab123"));
        assert!(!m("[^a-c]", false, "abc"));
        assert!(m("a$b", false, "a$b"));
    }

    #[test]
    fn extended_syntax() {
        assert!(m("(foo|bar)+x", true, "barfoox"));
        assert!(m("a{2,3}", true, "caad"));
        assert!(!m("^a{2,3}$", true, "aaaa"));
        assert!(m("a|b", true, "b"));
        assert!(m("a+?", true, "a"));
        assert!(m("x{", true, "x{"));
        assert!(m("\\<is\\>", true, "this is it"));
        assert!(!m("\\<is\\>", true, "this"));
    }

    #[test]
    fn case_and_words() {
        let re = Regex::new(b"foo", false, true).expect("compiles");
        assert!(re.is_match(b"xFOOx", false));
        assert!(!re.is_match(b"xFOOx", true));
        assert!(re.is_match(b"a FOO b", true));
        assert!(Regex::new(b"(", true, false).is_err());
        assert!(Regex::new(b"[a", false, false).is_err());
    }

    #[test]
    fn find_from_is_leftmost_longest() {
        let re = Regex::new(b"fo*", true, false).expect("compiles");
        assert_eq!(re.find_from(b"a foo fooo", 0, false), Some((2, 5)));
        assert_eq!(re.find_from(b"a foo fooo", 5, false), Some((6, 10)));
        assert_eq!(re.find_from(b"a foo", 5, false), None);
        let anchored = Regex::new(b"^foo", true, false).expect("compiles");
        assert_eq!(anchored.find_from(b"foo foo", 0, false), Some((0, 3)));
        assert_eq!(anchored.find_from(b"foo foo", 3, false), None);
        let word = Regex::new(b"foo", true, false).expect("compiles");
        assert_eq!(word.find_from(b"xfoo foo foox", 0, true), Some((5, 8)));
        let empty = Regex::new(b"o*", true, false).expect("compiles");
        assert_eq!(empty.find_from(b"foo", 0, false), Some((0, 0)));
        assert_eq!(empty.find_from(b"foo", 0, true), None);
        assert_eq!(empty.find_from(b"a oo", 0, true), Some((2, 4)));
    }
}
