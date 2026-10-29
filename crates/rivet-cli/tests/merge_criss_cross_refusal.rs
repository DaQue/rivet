//! A merge whose several merge bases conflict with each other in a way the virtual base cannot
//! render (here distinct types: an executable file against a symlink) is refused by name: taking
//! one base would commit a tree git does not. (When the bases merge cleanly the merge works,
//! `s_merge_criss_cross`; when they conflict in content, the virtual base keeps the markers like
//! git's, `s_merge_criss_cross_markers`; modify/delete and binary conflicts keep the base's version,
//! `s_merge_criss_cross_nontext`.)

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
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("f"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&dir, &["commit", "-qam", "x1"]);
    git(&dir, &["checkout", "-q", "-b", "y", "main"]);
    git(&dir, &["rm", "-q", "f"]);
    std::os::unix::fs::symlink("a", dir.join("f")).unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "y1"]);
    let x1 = git(&dir, &["rev-parse", "x"]);
    let y1 = git(&dir, &["rev-parse", "y"]);
    git(&dir, &["checkout", "-q", "x"]);
    git(&dir, &["merge", "-q", "-s", "ours", "--no-edit", &y1]);
    git(&dir, &["checkout", "-q", "y"]);
    git(&dir, &["merge", "-q", "-s", "ours", "--no-edit", &x1]);
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
