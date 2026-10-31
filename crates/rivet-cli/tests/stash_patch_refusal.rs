//! `stash push -p` and `stash save -p` (interactive hunk selection) are refused by name and stash
//! nothing; git would ask which hunks to take, so there is no differential scenario for them. The
//! rest of `stash` is the scenarios `s_stash*`.

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
fn patch_mode_is_refused_by_name_and_stashes_nothing() {
    let dir = std::env::temp_dir().join(format!("rivet-stash-patch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("a"), "a\n").unwrap();
    assert!(run(&dir, &["add", "a"]).status.success());
    assert!(run(&dir, &["commit", "-q", "-m", "one"]).status.success());
    std::fs::write(dir.join("a"), "a\nb\n").unwrap();

    for args in [
        &["stash", "push", "-p"][..],
        &["stash", "push", "--patch"][..],
        &["stash", "save", "-p"][..],
    ] {
        let out = run(&dir, args);
        assert_eq!(out.status.code(), Some(128), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not supported by rivet"), "{args:?}: {err}");
    }
    assert!(run(&dir, &["stash", "list"]).stdout.is_empty());
    assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"a\nb\n");
    let _ = std::fs::remove_dir_all(&dir);
}
