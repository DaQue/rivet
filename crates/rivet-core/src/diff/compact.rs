//! Change compaction: Git's `xdl_change_compact`, the pass that runs after the diff algorithm
//! and slides each group of changed lines to where a human would put it.
//!
//! A group of added or deleted lines is often ambiguous: in `a b b c` → `a b c`, deleting either
//! `b` is equivalent. Git first slides every group as far up and down as it will go (merging with
//! neighbours it bumps into, and lining up with groups in the other file), and then, when there
//! is still freedom, picks the position whose surrounding lines look like a block boundary using
//! the *indent heuristic*. Output only matches Git's when this pass matches too.

const MAX_INDENT: i32 = 200;
const MAX_BLANKS: i32 = 20;
const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;
const INDENT_HEURISTIC_MAX_SLIDING: isize = 100;

/// What the compaction needs to know about one file's lines.
pub struct Lines<'a> {
    /// Number of lines.
    pub count: usize,
    /// Whether line `i` equals line `j` of the same file.
    pub equal: &'a dyn Fn(usize, usize) -> bool,
    /// The indentation of line `i` in columns, or -1 for a blank line.
    pub indent: &'a dyn Fn(usize) -> i32,
}

/// The indentation of a line, measured as Git does (tabs to multiples of 8), or -1 if it holds
/// only whitespace.
pub fn indent_of(line: &[u8]) -> i32 {
    let mut indent = 0i32;
    for &byte in line {
        match byte {
            b' ' => indent += 1,
            b'\t' => indent += 8 - indent % 8,
            b'\n' | b'\r' | 0x0b | 0x0c => {}
            _ => return indent,
        }
        if indent >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1
}

#[derive(Clone, Copy)]
struct Group {
    start: isize,
    end: isize,
}

struct Side<'a, 'b> {
    lines: &'b Lines<'a>,
    changed: &'b mut Vec<bool>,
}

impl Side<'_, '_> {
    fn flag(&self, index: isize) -> bool {
        index >= 0 && (index as usize) < self.changed.len() && self.changed[index as usize]
    }

    fn set(&mut self, index: isize, value: bool) {
        self.changed[index as usize] = value;
    }

    fn init(&self) -> Group {
        let mut group = Group { start: 0, end: 0 };
        while self.flag(group.end) {
            group.end += 1;
        }
        group
    }

    /// Move to the next group; `false` when there is none.
    fn next(&self, group: &mut Group) -> bool {
        if group.end as usize == self.lines.count {
            return false;
        }
        group.start = group.end + 1;
        group.end = group.start;
        while self.flag(group.end) {
            group.end += 1;
        }
        true
    }

    fn previous(&self, group: &mut Group) -> bool {
        if group.start == 0 {
            return false;
        }
        group.end = group.start - 1;
        group.start = group.end;
        while self.flag(group.start - 1) {
            group.start -= 1;
        }
        true
    }

    /// Slide the group down by one line if the line below equals its first line.
    fn slide_down(&mut self, group: &mut Group) -> bool {
        if (group.end as usize) < self.lines.count
            && (self.lines.equal)(group.start as usize, group.end as usize)
        {
            self.set(group.start, false);
            group.start += 1;
            self.set(group.end, true);
            group.end += 1;
            while self.flag(group.end) {
                group.end += 1;
            }
            true
        } else {
            false
        }
    }

    fn slide_up(&mut self, group: &mut Group) -> bool {
        if group.start > 0
            && (self.lines.equal)((group.start - 1) as usize, (group.end - 1) as usize)
        {
            group.start -= 1;
            self.set(group.start, true);
            group.end -= 1;
            self.set(group.end, false);
            while self.flag(group.start - 1) {
                group.start -= 1;
            }
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct Measurement {
    end_of_file: bool,
    indent: i32,
    pre_blank: i32,
    pre_indent: i32,
    post_blank: i32,
    post_indent: i32,
}

fn measure_split(lines: &Lines<'_>, split: isize) -> Measurement {
    let mut m = Measurement::default();
    if split as usize >= lines.count {
        m.end_of_file = true;
        m.indent = -1;
    } else {
        m.indent = (lines.indent)(split as usize);
    }
    m.pre_indent = -1;
    let mut i = split - 1;
    while i >= 0 {
        m.pre_indent = (lines.indent)(i as usize);
        if m.pre_indent != -1 {
            break;
        }
        m.pre_blank += 1;
        if m.pre_blank == MAX_BLANKS {
            m.pre_indent = 0;
            break;
        }
        i -= 1;
    }
    m.post_indent = -1;
    let mut i = split + 1;
    while (i as usize) < lines.count {
        m.post_indent = (lines.indent)(i as usize);
        if m.post_indent != -1 {
            break;
        }
        m.post_blank += 1;
        if m.post_blank == MAX_BLANKS {
            m.post_indent = 0;
            break;
        }
        i += 1;
    }
    m
}

#[derive(Default, Clone, Copy)]
struct Score {
    effective_indent: i32,
    penalty: i32,
}

fn score_add_split(m: &Measurement, s: &mut Score) {
    if m.pre_indent == -1 && m.pre_blank == 0 {
        s.penalty += START_OF_FILE_PENALTY;
    }
    if m.end_of_file {
        s.penalty += END_OF_FILE_PENALTY;
    }
    let post_blank = if m.indent == -1 { 1 + m.post_blank } else { 0 };
    let total_blank = m.pre_blank + post_blank;
    s.penalty += TOTAL_BLANK_WEIGHT * total_blank;
    s.penalty += POST_BLANK_WEIGHT * post_blank;
    let indent = if m.indent != -1 {
        m.indent
    } else {
        m.post_indent
    };
    let any_blanks = total_blank != 0;
    s.effective_indent += indent;
    if indent == -1 || m.pre_indent == -1 {
        // nothing more to add
    } else if indent > m.pre_indent {
        s.penalty += if any_blanks {
            RELATIVE_INDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_INDENT_PENALTY
        };
    } else if indent == m.pre_indent {
        // same level as the line before
    } else if m.post_indent != -1 && m.post_indent > indent {
        s.penalty += if any_blanks {
            RELATIVE_OUTDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_OUTDENT_PENALTY
        };
    } else {
        s.penalty += if any_blanks {
            RELATIVE_DEDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_DEDENT_PENALTY
        };
    }
}

fn score_cmp(a: &Score, b: &Score) -> i32 {
    let cmp_indents = (a.effective_indent > b.effective_indent) as i32
        - (a.effective_indent < b.effective_indent) as i32;
    INDENT_WEIGHT * cmp_indents + (a.penalty - b.penalty)
}

/// Compact the changes of `changed` (in the file described by `lines`), using `other_changed` of
/// the other file to keep the two sides' groups aligned. Call it once for each file.
pub fn change_compact(
    lines: &Lines<'_>,
    changed: &mut Vec<bool>,
    other_lines: &Lines<'_>,
    other_changed: &mut Vec<bool>,
    indent_heuristic: bool,
) {
    let mut file = Side { lines, changed };
    let other = Side {
        lines: other_lines,
        changed: other_changed,
    };
    let mut g = file.init();
    let mut go = other.init();

    loop {
        if g.end != g.start {
            let mut earliest_end;
            let mut end_matching_other: isize;
            let mut groupsize;
            loop {
                groupsize = g.end - g.start;
                end_matching_other = -1;
                while file.slide_up(&mut g) {
                    other.previous(&mut go);
                }
                earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = g.end;
                }
                loop {
                    if !file.slide_down(&mut g) {
                        break;
                    }
                    other.next(&mut go);
                    if go.end > go.start {
                        end_matching_other = g.end;
                    }
                }
                if groupsize == g.end - g.start {
                    break;
                }
            }

            if g.end == earliest_end {
                // no shifting was possible
            } else if end_matching_other != -1 {
                while go.end == go.start {
                    file.slide_up(&mut g);
                    other.previous(&mut go);
                }
            } else if indent_heuristic {
                let mut shift = earliest_end;
                if g.end - groupsize - 1 > shift {
                    shift = g.end - groupsize - 1;
                }
                if g.end - INDENT_HEURISTIC_MAX_SLIDING > shift {
                    shift = g.end - INDENT_HEURISTIC_MAX_SLIDING;
                }
                let mut best_shift: isize = -1;
                let mut best = Score::default();
                while shift <= g.end {
                    let mut score = Score::default();
                    score_add_split(&measure_split(file.lines, shift), &mut score);
                    score_add_split(&measure_split(file.lines, shift - groupsize), &mut score);
                    if best_shift == -1 || score_cmp(&score, &best) <= 0 {
                        best = score;
                        best_shift = shift;
                    }
                    shift += 1;
                }
                while g.end > best_shift {
                    file.slide_up(&mut g);
                    other.previous(&mut go);
                }
            }
        }

        if !file.next(&mut g) {
            break;
        }
        other.next(&mut go);
    }
}
