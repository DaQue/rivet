//! `grep --break` over a binary file that matches is refused by name: git's output for it depends on
//! its worker threads and on whether a revision is searched. (Everything else about `--break` and
//! `-z` is the differential scenario `s_grep_null_break`.)

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
fn grep_break_refuses_binary() {
    let dir = std::env::temp_dir().join(format!("rivet-grep-break-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("bin.dat"), b"a\0foo\n").unwrap();
    std::fs::write(dir.join("a.txt"), b"foo\n").unwrap();
    assert!(run(&dir, &["add", "."]).status.success());

    let out = run(&dir, &["grep", "--break", "foo"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--break with a binary file that matches is not implemented"),
        "{err}"
    );

    // Without a binary match, or with the binary file handled as text, it still works.
    let ok = run(&dir, &["grep", "--break", "foo", "a.txt"]);
    assert!(ok.status.success());
    let text = run(&dir, &["grep", "--break", "-a", "foo"]);
    assert!(text.status.success());
    assert_eq!(text.stdout, b"a.txt:foo\n\nbin.dat:a\0foo\n");
}
