//! `symbolic-ref` with a one-level lowercase target is refused by name and writes nothing. Git
//! accepts it (`symbolic-ref FOO foo`), so there is no differential scenario for the refusal; the
//! rest of `symbolic-ref` is the scenario `s_symbolic_ref`.

use std::path::Path;
use std::process::{Command, Output};

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
fn a_one_level_target_is_refused_by_name_and_nothing_is_written() {
    let dir = scratch("symref-one-level");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    assert!(run(rivet, &dir, &["init", "-q"]).status.success());

    let out = run(rivet, &dir, &["symbolic-ref", "FOO", "foo"]);
    assert_eq!(out.status.code(), Some(128));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not implemented") && stderr.contains("'foo'"),
        "unexpected stderr: {stderr}"
    );
    assert!(!dir.join(".git/FOO").exists());

    // A multi-level target still works, so the refusal is not a blanket one.
    let ok = run(rivet, &dir, &["symbolic-ref", "FOO", "refs/heads/main"]);
    assert!(ok.status.success());
    assert_eq!(
        std::fs::read_to_string(dir.join(".git/FOO")).unwrap(),
        "ref: refs/heads/main\n"
    );
}
