//! `diff-tree` refuses the combined-diff formats it does not implement by name and prints nothing.
//! Git would print them, so there is no differential scenario for the refusal; `-c` and `--cc`
//! themselves are the scenario `s_diff_tree_combined`, the rest of `diff-tree` `s_diff_tree`.

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rivet"))
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

#[test]
fn unimplemented_combined_formats_are_refused_by_name_and_print_nothing() {
    let dir = std::env::temp_dir().join(format!("rivet-diff-tree-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("a"), "a\n").unwrap();
    assert!(run(&dir, &["add", "a"]).status.success());
    assert!(run(&dir, &["commit", "-q", "-m", "one"]).status.success());
    assert!(
        run(&dir, &["checkout", "-q", "-b", "side"])
            .status
            .success()
    );
    std::fs::write(dir.join("a"), "a\nside\n").unwrap();
    assert!(
        run(&dir, &["commit", "-q", "-a", "-m", "side"])
            .status
            .success()
    );
    assert!(run(&dir, &["checkout", "-q", "-"]).status.success());
    std::fs::write(dir.join("b"), "b\n").unwrap();
    assert!(run(&dir, &["add", "b"]).status.success());
    assert!(run(&dir, &["commit", "-q", "-m", "main"]).status.success());
    assert!(run(&dir, &["merge", "-q", "side"]).status.success());

    for args in [
        ["diff-tree", "-c", "--patch-with-raw", "HEAD"],
        ["diff-tree", "--cc", "--name-only", "HEAD"],
        ["diff-tree", "--cc", "--name-status", "HEAD"],
    ] {
        let out = run(&dir, &args);
        assert_eq!(out.status.code(), Some(128), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}: stdout must stay empty");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("with a combined diff is not implemented"),
            "{args:?}: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
