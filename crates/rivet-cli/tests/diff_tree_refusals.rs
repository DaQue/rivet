//! `diff-tree` refuses the options it does not implement by name and prints nothing. Git would
//! print combined diffs or detect renames, so there is no differential
//! scenario for the refusal; the rest of `diff-tree` is the scenario `s_diff_tree`.

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
fn unimplemented_options_are_refused_by_name_and_print_nothing() {
    let dir = std::env::temp_dir().join(format!("rivet-diff-tree-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("a"), "a\n").unwrap();
    assert!(run(&dir, &["add", "a"]).status.success());
    assert!(run(&dir, &["commit", "-q", "-m", "one"]).status.success());

    for option in ["-c", "--cc", "-M", "-C", "--find-copies-harder"] {
        let out = run(&dir, &["diff-tree", "-r", "--root", option, "HEAD"]);
        assert_eq!(out.status.code(), Some(128), "{option}");
        assert!(out.stdout.is_empty(), "{option}: stdout must stay empty");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!("diff-tree: {option} is not implemented")),
            "{option}: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
