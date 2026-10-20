//! The edit script must actually transform one sequence into the other.
//!
//! A diff that renders plausible hunks but whose edit indices are wrong is worse than no diff: it
//! compiles, it prints, and it silently changes the wrong lines when applied. These tests replay
//! the script against the inputs and check the result, and compare the edit count with a
//! brute-force LCS — the minimality Myers promises — on inputs small enough to verify by hand.

use rivet_core::diff::myers_diff;

/// Apply an edit script to `old`, panicking if the indices do not line up.
fn replay(old: &[&str], new: &[&str], edits: &[rivet_core::diff::Edit]) -> Vec<String> {
    let mut out = Vec::new();
    let mut oi = 0usize;
    let mut ni = 0usize;
    for edit in edits {
        if edit.old_len > 0 && edit.new_len > 0 {
            assert_eq!(edit.old_len, edit.new_len, "a match run must be equal on both sides");
            assert_eq!(edit.old_index, oi, "match run starts at the wrong old index");
            assert_eq!(edit.new_index, ni, "match run starts at the wrong new index");
            for k in 0..edit.old_len {
                assert_eq!(old[oi + k], new[ni + k], "a match run paired unequal lines");
                out.push(old[oi + k].to_string());
            }
            oi += edit.old_len;
            ni += edit.new_len;
        } else if edit.old_len > 0 {
            assert_eq!(edit.old_index, oi, "deletion starts at the wrong old index");
            oi += edit.old_len;
        } else if edit.new_len > 0 {
            assert_eq!(edit.new_index, ni, "insertion starts at the wrong new index");
            for k in 0..edit.new_len {
                out.push(new[ni + k].to_string());
            }
            ni += edit.new_len;
        }
    }
    assert_eq!(oi, old.len(), "script did not consume the old sequence");
    assert_eq!(ni, new.len(), "script did not consume the new sequence");
    out
}

fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    dp[0][0]
}

fn check(a: &[&str], b: &[&str]) {
    let edits = myers_diff(a, b);
    let rebuilt = replay(a, b, &edits);
    assert_eq!(rebuilt, b.to_vec(), "edit script does not reproduce the new sequence");
    let edits_used: usize = edits.iter().map(|e| e.old_len.max(e.new_len) - e.old_len.min(e.new_len)).sum();
    let minimal = a.len() + b.len() - 2 * lcs_len(a, b);
    assert_eq!(edits_used, minimal, "edit script is not minimal for {:?} -> {:?}", a, b);
}

#[test]
fn transforms_simple_replacements() {
    check(&["a", "b", "c"], &["X", "b", "c"]);
    check(&["a", "b", "c"], &["a", "b", "Y"]);
    check(&["a", "b", "c"], &["a", "X", "c"]);
}

#[test]
fn transforms_insertions_and_deletions() {
    check(&["a", "b", "c"], &["a", "b", "x", "y", "c"]);
    check(&["a", "b", "c"], &["a", "c"]);
    check(&[], &["one", "two"]);
    check(&["one", "two"], &[]);
    check(&[], &[]);
}

#[test]
fn transforms_the_paper_example() {
    check(
        &["a", "b", "c", "a", "b", "b", "a"],
        &["c", "b", "a", "b", "a", "c"],
    );
}

#[test]
fn handles_files_with_no_common_lines() {
    check(&["a", "b"], &["x", "y", "z"]);
}

#[test]
fn handles_identical_inputs() {
    let lines = ["a", "b", "c"];
    check(&lines, &lines);
}

#[test]
fn handles_a_long_common_prefix() {
    let mut a: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
    let mut b = a.clone();
    b[199] = "changed".to_string();
    let a_refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let b_refs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    check(&a_refs, &b_refs);
    a.truncate(0);
    let _ = a;
}
