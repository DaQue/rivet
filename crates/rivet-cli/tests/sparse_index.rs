//! The sparse index is refused by name, never half-handled. (The sparse definition itself is the
//! differential scenarios `s_sparse_*`.)

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

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn sparse_index_flag_is_refused_before_anything_changes() {
    let dir = scratch("sparse-index-flag");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    assert!(run(rivet, &dir, &["init", "-q"]).status.success());
    let before = std::fs::read(dir.join(".git/config")).unwrap();
    for args in [
        &["sparse-checkout", "set", "--sparse-index", "a"][..],
        &["sparse-checkout", "init", "--cone", "--sparse-index"][..],
    ] {
        let out = run(rivet, &dir, args);
        assert!(!out.status.success());
        assert_ne!(out.status.code(), Some(0));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--sparse-index is not implemented"),
            "{stderr}"
        );
    }
    assert_eq!(std::fs::read(dir.join(".git/config")).unwrap(), before);
    assert!(!dir.join(".git/config.worktree").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_index_written_as_sparse_by_git_is_refused_not_misread() {
    let dir = scratch("sparse-index-read");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    assert!(run("git", &dir, &["init", "-q"]).status.success());
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b/c")).unwrap();
    std::fs::write(dir.join("a/x"), "1\n").unwrap();
    std::fs::write(dir.join("b/c/y"), "2\n").unwrap();
    std::fs::write(dir.join("top"), "3\n").unwrap();
    assert!(run("git", &dir, &["add", "-A"]).status.success());
    assert!(
        run("git", &dir, &["commit", "-q", "-m", "i"])
            .status
            .success()
    );
    let set = run(
        "git",
        &dir,
        &[
            "-c",
            "advice.sparseIndexExpanded=false",
            "sparse-checkout",
            "set",
            "--cone",
            "--sparse-index",
            "a",
        ],
    );
    assert!(set.status.success());
    let out = run(rivet, &dir, &["status", "--short"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sparse index"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
