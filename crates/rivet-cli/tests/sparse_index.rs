//! The sparse index (`sdir`): rivet reads the one git writes and writes one git accepts, and the raw
//! `Index::load` still refuses it rather than misreading directory entries as files. (The
//! day-to-day behaviour is the scenario `s_sparse_index`; the sparse definition `s_sparse_*`.)

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

fn repo_with_dirs(dir: &Path) {
    assert!(run("git", dir, &["init", "-q"]).status.success());
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b/c")).unwrap();
    std::fs::write(dir.join("a/x"), "1\n").unwrap();
    std::fs::write(dir.join("b/c/y"), "2\n").unwrap();
    std::fs::write(dir.join("top"), "3\n").unwrap();
    assert!(run("git", dir, &["add", "-A"]).status.success());
    assert!(
        run("git", dir, &["commit", "-q", "-m", "i"])
            .status
            .success()
    );
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn an_index_written_as_sparse_by_git_reads_as_the_full_index() {
    let dir = scratch("sparse-index-read");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    repo_with_dirs(&dir);
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
    let sparse = text(&run("git", &dir, &["ls-files", "--sparse", "-s"]));
    assert!(
        sparse.contains("040000") && sparse.contains("\tb/"),
        "{sparse}"
    );

    let full = text(&run(
        "git",
        &dir,
        &["-c", "advice.sparseIndexExpanded=false", "ls-files", "-s"],
    ));
    assert_eq!(text(&run(rivet, &dir, &["ls-files", "-s"])), full);
    let status = run(rivet, &dir, &["status", "--short"]);
    assert!(
        status.status.success() && status.stdout.is_empty(),
        "{}",
        text(&status)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rivet_writes_the_sparse_index_git_expects() {
    let dir = scratch("sparse-index-write");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    repo_with_dirs(&dir);
    // `--sparse-index` alone means cone mode, as in git.
    let out = run(
        rivet,
        &dir,
        &["sparse-checkout", "set", "--sparse-index", "a"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sparse = text(&run("git", &dir, &["ls-files", "--sparse", "-s"]));
    assert!(
        sparse.contains("040000") && sparse.contains("\tb/"),
        "{sparse}"
    );
    assert!(
        sparse.contains("\ta/x") && !sparse.contains("b/c/y"),
        "{sparse}"
    );
    assert!(run("git", &dir, &["fsck", "--strict"]).status.success());

    // `--no-sparse-index` turns the same index back into a full one.
    let out = run(
        rivet,
        &dir,
        &["sparse-checkout", "set", "--cone", "--no-sparse-index", "a"],
    );
    assert!(out.status.success());
    let sparse = text(&run("git", &dir, &["ls-files", "--sparse", "-s"]));
    assert!(
        sparse.contains("b/c/y") && !sparse.contains("040000"),
        "{sparse}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_raw_index_loader_still_refuses_a_sparse_index() {
    let dir = scratch("sparse-index-raw");
    repo_with_dirs(&dir);
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
    let bytes = std::fs::read(dir.join(".git/index")).unwrap();
    assert!(bytes.windows(4).any(|w| w == b"sdir"));
    let err = rivet_core::Index::load(&dir.join(".git/index"), rivet_core::ObjectFormat::Sha1)
        .unwrap_err();
    assert!(
        matches!(err, rivet_core::Error::NotImplemented { .. }),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
