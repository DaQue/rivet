//! `blame -L :<funcname>` with a built-in userdiff driver (`diff=cpp` and friends) is refused by
//! name: git would apply that driver's function patterns, which rivet does not carry. The default
//! rule, a configured `diff.<driver>.xfuncname` and plain ranges still work (`s_blame_funcname`).

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

fn git(dir: &Path, args: &[&str]) {
    let out = run("/usr/bin/git", dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn builtin_driver_patterns_are_refused_and_plain_ranges_still_work() {
    let dir = std::env::temp_dir().join(format!("rivet-blame-func-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join(".gitattributes"), "*.c diff=cpp\n").unwrap();
    std::fs::write(dir.join("f.c"), "int foo(void)\n{\n}\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);

    let rivet = env!("CARGO_BIN_EXE_rivet");
    let out = run(rivet, &dir, &["blame", "-L", ":foo", "f.c"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("diff=cpp") && err.contains("not implemented"),
        "{err}"
    );

    let out = run(rivet, &dir, &["blame", "-L", "1,2", "f.c"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
