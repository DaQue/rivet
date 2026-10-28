//! `update-ref --stdin` refuses the `symref-*` commands by name and writes nothing. Git accepts
//! them, so there is no differential scenario for the refusal; the rest of `update-ref` is the
//! scenario `s_update_ref`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(program: &str, dir: &Path, args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_COMMITTER_NAME", "n")
        .env("GIT_COMMITTER_EMAIL", "n@e")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().expect("wait")
}

#[test]
fn symref_commands_are_refused_by_name_and_write_nothing() {
    let dir = std::env::temp_dir().join(format!("rivet-update-ref-symref-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rivet = env!("CARGO_BIN_EXE_rivet");
    assert!(run(rivet, &dir, &["init", "-q"], "").status.success());

    for command in [
        "symref-update",
        "symref-create",
        "symref-delete",
        "symref-verify",
    ] {
        let input = format!("{command} refs/heads/s refs/heads/c\n");
        let out = run(rivet, &dir, &["update-ref", "--stdin"], &input);
        assert_eq!(out.status.code(), Some(128), "{command}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not implemented") && stderr.contains(command),
            "unexpected stderr for {command}: {stderr}"
        );
        assert!(!dir.join(".git/refs/heads/s").exists(), "{command}");
    }
}
