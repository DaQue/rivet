//! Git's combined diff (`diff --cc`) of a work tree file against the two sides of a conflict.
//!
//! # Caveats
//!
//! * **This is a port of the shape of `combine-diff.c`, not of its call graph.** Each parent is
//!   diffed against the result with no context; the lost (removed) lines are attached to the
//!   result line they precede, identical lost lines of consecutive parents are squashed into one
//!   line marked for both, and the dense filter then drops every hunk in which the result agrees
//!   with all parents but one. That filter is what makes a resolved conflict print nothing.
//! * **Only two parents are supported** (stages 2 and 3), which is all `git diff` ever combines
//!   for an unmerged path.
//! * **No function-context comment is appended to the `@@@` line**, which is what `git diff`
//!   prints for a combined diff.
//! * **A line without a terminator is printed with one and no `\ No newline` marker**, which is
//!   what `git diff` does for a combined diff.

use crate::diff::myers::{DiffLine, compact_edits, git_diff, split_lines};

/// One line removed from at least one parent and absent from the result.
struct Lost {
    line: Vec<u8>,
    map: u32,
}

/// One result line (plus the lost lines that precede it).
struct Sline<'a> {
    text: &'a [u8],
    flag: u32,
    lost: Vec<Lost>,
    cursor: usize,
    pending: Vec<Vec<u8>>,
    p_lno: Vec<usize>,
}

fn find_next(s: &[Sline<'_>], mark: u32, mut i: usize, cnt: usize, uninteresting: bool) -> usize {
    while i <= cnt {
        let marked = s[i].flag & mark != 0;
        if marked != uninteresting {
            return i;
        }
        i += 1;
    }
    cnt + 1
}

fn adjust_hunk_tail(s: &[Sline<'_>], all_mask: u32, hunk_begin: usize, i: usize) -> usize {
    if hunk_begin < i && s[i - 1].flag & all_mask == 0 {
        i - 1
    } else {
        i
    }
}

fn give_context(s: &mut [Sline<'_>], cnt: usize, np: usize, context: usize) {
    let all_mask = (1u32 << np) - 1;
    let mark = 1u32 << np;
    let no_pre_delete = 2u32 << np;
    let mut i = find_next(s, mark, 0, cnt, false);
    if cnt < i {
        return;
    }
    while i <= cnt {
        let mut j = i.saturating_sub(context);
        while j < i {
            s[j].flag |= mark | no_pre_delete;
            j += 1;
        }
        loop {
            j = find_next(s, mark, i, cnt, true);
            if cnt < j {
                return;
            }
            let k = find_next(s, mark, j, cnt, false);
            j = adjust_hunk_tail(s, all_mask, i, j);
            if k < j + context {
                while j < k {
                    s[j].flag |= mark;
                    j += 1;
                }
                i = k;
                continue;
            }
            let end = if j + context < cnt + 1 {
                j + context
            } else {
                cnt + 1
            };
            while j < end {
                s[j].flag |= mark;
                j += 1;
            }
            i = find_next(s, mark, end, cnt, false);
            break;
        }
    }
}

fn make_hunks(s: &mut [Sline<'_>], cnt: usize, np: usize, context: usize) {
    let all_mask = (1u32 << np) - 1;
    let mark = 1u32 << np;
    for line in s.iter_mut().take(cnt + 1) {
        if line.flag & all_mask != 0 || !line.lost.is_empty() {
            line.flag |= mark;
        } else {
            line.flag &= !mark;
        }
    }
    let mut i = 0;
    while i <= cnt {
        while i <= cnt && s[i].flag & mark == 0 {
            i += 1;
        }
        if cnt < i {
            break;
        }
        let hunk_begin = i;
        let mut j = i + 1;
        while j <= cnt {
            if s[j].flag & mark == 0 {
                let mut la = adjust_hunk_tail(s, all_mask, hunk_begin, j);
                la = if la + context < cnt + 1 {
                    la + context
                } else {
                    cnt
                };
                let mut contin = false;
                while la > 0 {
                    la -= 1;
                    if j > la {
                        break;
                    }
                    if s[la].flag & mark != 0 {
                        contin = true;
                        break;
                    }
                }
                if !contin {
                    break;
                }
                j = la;
            }
            j += 1;
        }
        let hunk_end = j;
        let mut same_diff = 0u32;
        let mut has_interesting = false;
        let mut k = i;
        while k < hunk_end && !has_interesting {
            let this_diff = s[k].flag & all_mask;
            if this_diff != 0 {
                if same_diff == 0 {
                    same_diff = this_diff;
                } else if same_diff != this_diff {
                    has_interesting = true;
                    break;
                }
            }
            for ll in &s[k].lost {
                if has_interesting {
                    break;
                }
                if same_diff == 0 {
                    same_diff = ll.map;
                } else if same_diff != ll.map {
                    has_interesting = true;
                }
            }
            k += 1;
        }
        if !has_interesting && same_diff != all_mask {
            for line in &mut s[hunk_begin..hunk_end.min(cnt + 1)] {
                line.flag &= !mark;
            }
        }
        i = hunk_end;
    }
    give_context(s, cnt, np, context);
}

fn push_line(out: &mut Vec<u8>, prefix: &[u8], text: &[u8]) {
    out.extend_from_slice(prefix);
    out.extend_from_slice(text);
    if !text.ends_with(b"\n") {
        out.push(b'\n');
    }
}

/// The hunks of the combined diff of `result` against `parents`, or an empty vector when the
/// dense filter leaves nothing to show.
///
/// `parents` holds the parents' contents (stage 2, then stage 3); the returned bytes start at the
/// first `@@@` line.
pub fn combined_hunks(parents: &[&[u8]], result: &[u8], context: usize) -> Vec<u8> {
    let np = parents.len();
    let res_lines: Vec<DiffLine<'_>> = split_lines(result);
    let cnt = res_lines.len();
    let mut s: Vec<Sline<'_>> = (0..=cnt + 1)
        .map(|i| Sline {
            text: if i < cnt { res_lines[i].content } else { b"" },
            flag: 0,
            lost: Vec::new(),
            cursor: 0,
            pending: Vec::new(),
            p_lno: vec![0; np],
        })
        .collect();
    for (n, parent) in parents.iter().enumerate() {
        let nmask = 1u32 << n;
        let plines = split_lines(parent);
        let edits = git_diff(&plines, &res_lines);
        let edits = compact_edits(&plines, &res_lines, &edits);
        for line in s.iter_mut() {
            line.cursor = 0;
            line.pending.clear();
        }
        for edit in edits.iter().filter(|e| e.is_change()) {
            let bucket = edit.new_index;
            for p in &plines[edit.old_index..edit.old_index + edit.old_len] {
                let sl = &mut s[bucket];
                // Merge this parent's lost lines into the list by looking forward for the same
                // line: lines before the match that this parent lacks stay as they are, and the
                // lines it lost without a match are placed just before the next match.
                let found = (sl.cursor..sl.lost.len()).find(|&j| sl.lost[j].line == p.content);
                match found {
                    Some(j) => {
                        let at = j;
                        let pending = std::mem::take(&mut sl.pending);
                        let count = pending.len();
                        for (k, line) in pending.into_iter().enumerate() {
                            sl.lost.insert(at + k, Lost { line, map: nmask });
                        }
                        sl.lost[at + count].map |= nmask;
                        sl.cursor = at + count + 1;
                    }
                    None => sl.pending.push(p.content.to_vec()),
                }
            }
            for line in &mut s[edit.new_index..edit.new_index + edit.new_len] {
                line.flag |= nmask;
            }
        }
        for line in s.iter_mut() {
            for text in std::mem::take(&mut line.pending) {
                line.lost.push(Lost {
                    line: text,
                    map: nmask,
                });
            }
        }
        let mut p_lno = 1usize;
        for (lno, line) in s.iter_mut().enumerate().take(cnt + 1) {
            line.p_lno[n] = p_lno;
            for ll in &line.lost {
                if ll.map & nmask != 0 {
                    p_lno += 1;
                }
            }
            if lno < cnt && line.flag & nmask == 0 {
                p_lno += 1;
            }
        }
        s[cnt + 1].p_lno[n] = p_lno;
    }
    let all_mask = (1u32 << np) - 1;
    let mark = 1u32 << np;
    let no_pre_delete = 2u32 << np;
    let _ = all_mask;
    make_hunks(&mut s, cnt, np, context);

    let mut out = Vec::new();
    let mut lno = 0usize;
    loop {
        while lno <= cnt && s[lno].flag & mark == 0 {
            lno += 1;
        }
        if cnt < lno {
            break;
        }
        let mut hunk_end = lno + 1;
        while hunk_end <= cnt && s[hunk_end].flag & mark != 0 {
            hunk_end += 1;
        }
        let mut rlines = hunk_end - lno;
        if cnt < hunk_end {
            rlines -= 1;
        }
        let ats = "@".repeat(np + 1);
        out.extend_from_slice(ats.as_bytes());
        for n in 0..np {
            let l0 = s[lno].p_lno[n];
            let l1 = s[hunk_end].p_lno[n];
            out.extend_from_slice(format!(" -{},{}", l0, l1 - l0).as_bytes());
        }
        out.extend_from_slice(format!(" +{},{} ", lno + 1, rlines).as_bytes());
        out.extend_from_slice(ats.as_bytes());
        out.push(b'\n');
        while lno < hunk_end {
            let at = lno;
            lno += 1;
            if s[at].flag & no_pre_delete == 0 {
                for ll in &s[at].lost {
                    let prefix: Vec<u8> = (0..np)
                        .map(|j| if ll.map & (1 << j) != 0 { b'-' } else { b' ' })
                        .collect();
                    push_line(&mut out, &prefix, &ll.line);
                }
            }
            if cnt < lno {
                break;
            }
            let prefix: Vec<u8> = (0..np)
                .map(|j| {
                    if s[at].flag & (1 << j) != 0 {
                        b'+'
                    } else {
                        b' '
                    }
                })
                .collect();
            push_line(&mut out, &prefix, s[at].text);
        }
    }
    out
}
