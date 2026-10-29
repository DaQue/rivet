//! `update-index` implements the entry options (`--add`, `--remove`, `--refresh`, `--cacheinfo`,
//! `--index-info`, `--stdin`, `--chmod`, the assume-unchanged and skip-worktree bits) and the
//! index-format options; the options that remain are refused by name and leave the index file
//! untouched. The implemented part is the scenarios `s_update_index`, `s_split_index` and
//! `s_index_version`.

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run")
}

#[test]
fn unimplemented_options_are_refused_by_name_and_leave_the_index_alone() {
    let dir =
        std::env::temp_dir().join(format!("rivet-update-index-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("a"), "a\n").unwrap();
    assert!(run(&dir, &["add", "a"]).status.success());
    let before = std::fs::read(dir.join(".git/index")).unwrap();

    for option in [
        "--unresolve",
        "--again",
        "-g",
        "--untracked-cache",
        "--no-untracked-cache",
        "--test-untracked-cache",
        "--force-untracked-cache",
        "--fsmonitor",
        "--fsmonitor-valid",
        "--no-fsmonitor-valid",
    ] {
        let out = run(&dir, &["update-index", option]);
        assert_eq!(out.status.code(), Some(128), "{option}");
        assert!(out.stdout.is_empty(), "{option}: stdout must stay empty");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("update-index:") && err.contains("is not implemented"),
            "{option}: {err}"
        );
    }
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before);

    // A sparse-directory entry (mode 040000) is refused rather than turned into a gitlink.
    let out = run(
        &dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "040000,e69de29bb2d1d6434b8b29ae775ad8c2e48c5391,dir",
        ],
    );
    assert_eq!(out.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&out.stderr).contains("sparse directory entries"));
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}
