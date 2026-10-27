//! A merge with more than one merge base is refused by name: git builds a virtual base
//! recursively and may conflict where taking one base would commit a clean (wrong) tree.

use std::path::Path;
use std::process::{Command, Output};

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "n")
        .env("GIT_AUTHOR_EMAIL", "n@e")
        .env("GIT_COMMITTER_NAME", "n")
        .env("GIT_COMMITTER_EMAIL", "n@e")
        .output()
        .expect("spawn")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("/usr/bin/git", dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, file: &str, body: &str, msg: &str) {
    std::fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", msg]);
}

#[test]
fn merge_with_two_bases_is_refused() {
    let dir = std::env::temp_dir().join(format!("rivet-criss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    commit(&dir, "f", "a\n", "base");
    git(&dir, &["checkout", "-q", "-b", "x", "main"]);
    commit(&dir, "x", "x\n", "x1");
    git(&dir, &["checkout", "-q", "-b", "y", "main"]);
    commit(&dir, "y", "y\n", "y1");
    let x1 = git(&dir, &["rev-parse", "x"]);
    let y1 = git(&dir, &["rev-parse", "y"]);
    git(&dir, &["checkout", "-q", "x"]);
    git(&dir, &["merge", "-q", "--no-edit", &y1]);
    git(&dir, &["checkout", "-q", "y"]);
    git(&dir, &["merge", "-q", "--no-edit", &x1]);
    commit(&dir, "y2", "y2\n", "y2");
    git(&dir, &["checkout", "-q", "x"]);
    commit(&dir, "x2", "x2\n", "x2");
    let bases = git(&dir, &["merge-base", "--all", "x", "y"]);
    assert_eq!(bases.lines().count(), 2, "fixture must be criss-cross");

    let before = git(&dir, &["rev-parse", "HEAD"]);
    let out = run(
        env!("CARGO_BIN_EXE_rivet"),
        &dir,
        &["merge", "--no-edit", "y"],
    );
    assert_eq!(out.status.code(), Some(128));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("more than one merge base"), "{err}");
    for base in bases.lines() {
        assert!(err.contains(base), "{err}");
    }
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), before);
    assert!(git(&dir, &["status", "--porcelain"]).is_empty());
}
