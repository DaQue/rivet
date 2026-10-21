//! Phase 5 — ignore decisions compared with `git check-ignore` on a real repository.
//!
//! The interesting cases are the ones a naive matcher gets wrong: last-match-wins across files,
//! re-inclusion, an unanchored pattern matching at any depth, an anchored pattern matching only
//! from the root, and — the subtle one — a negated pattern that cannot rescue a path whose ancestor
//! directory is excluded.

use std::path::{Path, PathBuf};
use std::process::Command;

use bstr::BStr;
use rivet_core::ignore::{IgnoreStack, PathKind};

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether `git` considers `path` ignored, checked without consulting the index.
fn git_ignores(root: &Path, path: &str) -> bool {
    let output = Command::new("git")
        .current_dir(root)
        .args(["check-ignore", "--no-index", "-q", "--", path])
        .output()
        .expect("git check-ignore");
    match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!(
            "git check-ignore {path} exited {other:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn repository() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "--quiet", "-b", "main"]);

    std::fs::write(
        root.join(".gitignore"),
        "build/\n*.log\n!keep.log\ndocs/**/draft.md\n/top.txt\nsub/ignored-in-sub.txt\n",
    )
    .unwrap();
    std::fs::write(root.join("a.log"), "x\n").unwrap();
    std::fs::write(root.join("keep.log"), "x\n").unwrap();
    std::fs::write(root.join("top.txt"), "x\n").unwrap();
    std::fs::write(root.join("src-main.rs"), "x\n").unwrap();

    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::write(root.join("build/output.bin"), "x\n").unwrap();
    std::fs::write(root.join("build/keep.txt"), "x\n").unwrap();

    std::fs::create_dir_all(root.join("docs/a/b")).unwrap();
    std::fs::write(root.join("docs/a/b/draft.md"), "x\n").unwrap();
    std::fs::write(root.join("docs/a/b/final.md"), "x\n").unwrap();

    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/.gitignore"), "!*.log\nlocal.tmp\n").unwrap();
    std::fs::write(root.join("sub/x.log"), "x\n").unwrap();
    std::fs::write(root.join("sub/y.txt"), "x\n").unwrap();
    std::fs::write(root.join("sub/local.tmp"), "x\n").unwrap();
    std::fs::write(root.join("sub/ignored-in-sub.txt"), "x\n").unwrap();
    std::fs::write(root.join("sub/top.txt"), "x\n").unwrap();

    (dir, root)
}

#[test]
fn ignore_decisions_match_git_check_ignore() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    let mut stack = IgnoreStack::new(&root);
    stack.push_info_exclude(&root.join(".git")).unwrap();
    stack.push_dir(BStr::new("")).unwrap();
    stack.push_dir(BStr::new("sub")).unwrap();

    let cases: &[(&str, PathKind)] = &[
        ("build", PathKind::Directory),
        ("build/output.bin", PathKind::File),
        ("build/keep.txt", PathKind::File),
        ("a.log", PathKind::File),
        ("keep.log", PathKind::File),
        ("top.txt", PathKind::File),
        ("sub/top.txt", PathKind::File),
        ("src-main.rs", PathKind::File),
        ("docs/a/b/draft.md", PathKind::File),
        ("docs/a/b/final.md", PathKind::File),
        ("sub/x.log", PathKind::File),
        ("sub/y.txt", PathKind::File),
        ("sub/local.tmp", PathKind::File),
        ("sub/ignored-in-sub.txt", PathKind::File),
    ];

    let mut mismatches = Vec::new();
    for (path, kind) in cases {
        let expected = git_ignores(&root, path);
        let ours = stack.is_ignored(BStr::new(path), *kind).unwrap();
        if ours != expected {
            let detail = stack
                .check(BStr::new(path), *kind)
                .unwrap()
                .pattern()
                .map(|p| p.source.describe());
            mismatches.push(format!(
                "{path}: git={expected} ours={ours} decision={:?}",
                detail
            ));
        }
    }
    assert!(mismatches.is_empty(), "mismatches:\n{}", mismatches.join("\n"));
}

#[test]
fn exclusion_of_an_ancestor_beats_re_inclusion() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "--quiet", "-b", "main"]);
    std::fs::write(root.join(".gitignore"), "build/\n!build/keep.txt\n").unwrap();
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::write(root.join("build/keep.txt"), "x\n").unwrap();

    let mut stack = IgnoreStack::new(root);
    stack.push_info_exclude(&root.join(".git")).unwrap();
    stack.push_dir(BStr::new("")).unwrap();

    let ours = stack.is_ignored(BStr::new("build/keep.txt"), PathKind::File).unwrap();
    assert_eq!(ours, git_ignores(root, "build/keep.txt"));
    assert!(ours, "a negated pattern must not rescue a file inside an excluded directory");
}
