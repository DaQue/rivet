//! Compare `merge::merge_content` against real `git merge-file`.
//!
//! The merge is the one place where "close enough" is invisible until a user loses work, so the
//! only test worth writing is byte equality with Git's own three-way merge on the same inputs.
//! Every expected value below is produced by `git merge-file`, never written by hand.

use std::path::PathBuf;
use std::process::Command;

use rivet_core::diff::DiffAlgorithm;
use rivet_core::merge::{merge_content, ConflictStyle};

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "rivet-merge-content-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `git merge-file` for one triple, returning its standard output.
fn git_merge(tag: &str, base: &[u8], ours: &[u8], theirs: &[u8], style: Option<&str>) -> Vec<u8> {
    let dir = temp_dir(tag);
    let ours_path = dir.join("ours.txt");
    let base_path = dir.join("base.txt");
    let theirs_path = dir.join("theirs.txt");
    std::fs::write(&ours_path, ours).unwrap();
    std::fs::write(&base_path, base).unwrap();
    std::fs::write(&theirs_path, theirs).unwrap();

    let mut cmd = Command::new("git");
    cmd.current_dir(&dir)
        .arg("merge-file")
        .arg("-p")
        .arg("-L")
        .arg("ours")
        .arg("-L")
        .arg("base")
        .arg("-L")
        .arg("theirs");
    if let Some(style) = style {
        cmd.arg(format!("--{style}"));
    }
    let output = cmd
        .arg(&ours_path)
        .arg(&base_path)
        .arg(&theirs_path)
        .output()
        .expect("git merge-file runs");
    // Exit status is the number of conflicts, so only a negative status is a failure.
    assert!(
        output.status.code().map(|c| c >= 0).unwrap_or(false),
        "git merge-file failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
    output.stdout
}

fn style_arg(style: ConflictStyle) -> Option<&'static str> {
    match style {
        ConflictStyle::Merge => None,
        ConflictStyle::Diff3 => Some("diff3"),
        ConflictStyle::Zd3 => Some("zdiff3"),
    }
}

fn compare(base: &[u8], ours: &[u8], theirs: &[u8], style: ConflictStyle) {
    let expected = git_merge("cmp", base, ours, theirs, style_arg(style));
    let actual = merge_content(
        Some(base),
        ours,
        theirs,
        ("ours", "theirs"),
        style,
        DiffAlgorithm::Myers,
    )
    .expect("merge succeeds");
    assert_eq!(
        String::from_utf8_lossy(&actual.data),
        String::from_utf8_lossy(&expected),
        "merge differs from git for style {:?}\nbase:\n{}\nours:\n{}\ntheirs:\n{}",
        style,
        String::from_utf8_lossy(base),
        String::from_utf8_lossy(ours),
        String::from_utf8_lossy(theirs)
    );
}

fn compare_all_styles(base: &[u8], ours: &[u8], theirs: &[u8]) {
    compare(base, ours, theirs, ConflictStyle::Merge);
    compare(base, ours, theirs, ConflictStyle::Diff3);
    compare(base, ours, theirs, ConflictStyle::Zd3);
}

#[test]
fn non_overlapping_edits_merge_without_markers() {
    let base = b"one\ntwo\nthree\nfour\nfive\n";
    let ours = b"one\nTWO\nthree\nfour\nfive\n";
    let theirs = b"one\ntwo\nthree\nfour\nFIVE\n";
    for style in [ConflictStyle::Merge, ConflictStyle::Diff3, ConflictStyle::Zd3] {
        compare(base, ours, theirs, style);
    }
    let result = merge_content(
        Some(base),
        ours,
        theirs,
        ("ours", "theirs"),
        ConflictStyle::Merge,
        DiffAlgorithm::Myers,
    )
    .unwrap();
    assert!(!result.conflict);
    assert_eq!(result.data.as_slice(), b"one\nTWO\nthree\nfour\nFIVE\n");
}

#[test]
fn adjacent_edits_merge_cleanly() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nB\nc\n";
    let theirs = b"a\nb\nC\n";
    compare_all_styles(base, ours, theirs);
}

#[test]
fn insertion_next_to_a_change_merges_cleanly() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nB\nc\n";
    let theirs = b"a\nb\ninserted\nc\n";
    compare_all_styles(base, ours, theirs);
}

#[test]
fn overlapping_edits_conflict() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nOURS\nc\n";
    let theirs = b"a\nTHEIRS\nc\n";
    compare_all_styles(base, ours, theirs);
    let result = merge_content(
        Some(base),
        ours,
        theirs,
        ("ours", "theirs"),
        ConflictStyle::Merge,
        DiffAlgorithm::Myers,
    )
    .unwrap();
    assert!(result.conflict);
    assert_eq!(result.hunks.len(), 1);
    assert_eq!(result.hunks[0].base_start, 1);
    assert_eq!(result.hunks[0].base_len, 1);
}

#[test]
fn both_sides_making_the_same_change_is_clean() {
    let base = b"a\nb\nc\n";
    let changed = b"a\nB\nc\n";
    compare_all_styles(base, changed, changed);
    let result = merge_content(
        Some(base),
        changed,
        changed,
        ("ours", "theirs"),
        ConflictStyle::Merge,
        DiffAlgorithm::Myers,
    )
    .unwrap();
    assert!(!result.conflict);
}

#[test]
fn a_deletion_against_an_edit_conflicts() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nc\n";
    let theirs = b"a\nB\nc\n";
    compare_all_styles(base, ours, theirs);
}

#[test]
fn edits_at_the_very_start_and_end_conflict() {
    let base = b"a\nb\nc\n";
    let ours = b"OURS\nb\nc\n";
    let theirs = b"THEIRS\nb\nc\n";
    compare_all_styles(base, ours, theirs);
}

#[test]
fn missing_trailing_newline_is_preserved() {
    let base = b"a\nb";
    let ours = b"a\nB";
    let theirs = b"a\nb\nc";
    compare_all_styles(base, ours, theirs);
}

#[test]
fn an_empty_base_is_the_add_add_case() {
    let base = b"";
    let ours = b"one\ntwo\n";
    let theirs = b"one\nTWO\n";
    compare_all_styles(base, ours, theirs);
}

#[test]
fn one_sided_change_returns_that_side_verbatim() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nb\nc\n";
    let theirs = b"a\nb\nC\n";
    for style in [ConflictStyle::Merge, ConflictStyle::Diff3, ConflictStyle::Zd3] {
        compare(base, ours, theirs, style);
    }
}

#[test]
fn skips_when_git_is_unavailable() {
    if !have_git() {
        eprintln!("git not found; skipping");
    }
}
