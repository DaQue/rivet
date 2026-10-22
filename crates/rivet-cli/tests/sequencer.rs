//! Behaviour of `cherry-pick` / `revert` that has no counterpart in real Git's state: a sequence
//! that would stop on a conflict is refused before anything is written, and an empty result is
//! reported without leaving state behind. Everything else is covered by the differential
//! scenarios (`s_cherry_pick*`, `s_revert*`).

use std::path::Path;
use std::process::{Command, Output};

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("spawn")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("git", dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn rivet(dir: &Path, args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_rivet"), dir, args)
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), "1\n2\n3\n").unwrap();
    git(dir.path(), &["add", "f"]);
    git(dir.path(), &["commit", "-qm", "base"]);
    dir
}

#[test]
fn a_range_that_would_conflict_is_refused_untouched() {
    let dir = repo();
    let p = dir.path();
    git(p, &["branch", "topic"]);
    std::fs::write(p.join("f"), "1\nmain\n3\n").unwrap();
    git(p, &["commit", "-qam", "main edit"]);
    git(p, &["switch", "-q", "topic"]);
    std::fs::write(p.join("g"), "g\n").unwrap();
    git(p, &["add", "g"]);
    git(p, &["commit", "-qm", "topic adds g"]);
    std::fs::write(p.join("f"), "1\ntopic\n3\n").unwrap();
    git(p, &["commit", "-qam", "topic edits f"]);
    git(p, &["switch", "-q", "main"]);
    let head = git(p, &["rev-parse", "HEAD"]);

    let out = rivet(p, &["cherry-pick", "main..topic"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(git(p, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(p, &["status", "--porcelain"]), "");
    assert!(!p.join(".git/CHERRY_PICK_HEAD").exists());
    assert!(
        !p.join("g").exists(),
        "the first commit of the sequence must not have been applied"
    );
}

#[test]
fn an_empty_result_exits_1_and_changes_nothing() {
    let dir = repo();
    let p = dir.path();
    git(p, &["branch", "topic"]);
    std::fs::write(p.join("g"), "g\n").unwrap();
    git(p, &["add", "g"]);
    git(p, &["commit", "-qm", "adds g"]);
    git(p, &["switch", "-q", "topic"]);
    std::fs::write(p.join("g"), "g\n").unwrap();
    git(p, &["add", "g"]);
    git(p, &["commit", "-qm", "adds g too"]);
    git(p, &["switch", "-q", "main"]);
    git(p, &["merge", "-q", "--no-edit", "topic"]);
    let head = git(p, &["rev-parse", "HEAD"]);

    let out = rivet(p, &["cherry-pick", "topic"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(git(p, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(p, &["status", "--porcelain"]), "");
    assert!(!p.join(".git/CHERRY_PICK_HEAD").exists());
}
