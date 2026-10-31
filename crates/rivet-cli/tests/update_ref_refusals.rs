//! `update-ref --stdin` symref commands, driven without a `git` binary: the differential scenario
//! `s_update_ref_symref` compares every case against git, this pins what must hold on its own — the
//! file git expects on disk, and that nothing is written when a command is refused.

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
fn symref_commands_write_and_remove_symbolic_refs_and_refuse_without_writing() {
    let dir = std::env::temp_dir().join(format!("rivet-update-ref-symref-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rivet = env!("CARGO_BIN_EXE_rivet");
    assert!(run(rivet, &dir, &["init", "-q"], "").status.success());
    let stdin = |input: &str, extra: &[&str]| {
        let mut args = vec!["update-ref", "--stdin"];
        args.extend_from_slice(extra);
        run(rivet, &dir, &args, input)
    };

    let out = stdin("symref-create refs/heads/s refs/heads/c\n", &[]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out.stderr);
    assert_eq!(
        std::fs::read(dir.join(".git/refs/heads/s")).unwrap(),
        b"ref: refs/heads/c\n"
    );

    // a second create, a deref-mode delete and a wrong old target all leave it alone
    for (input, extra, text) in [
        (
            "symref-create refs/heads/s refs/heads/d\n",
            &[][..],
            "dangling symref already exists",
        ),
        (
            "symref-delete refs/heads/s\n",
            &[][..],
            "cannot operate with deref mode",
        ),
        (
            "symref-delete refs/heads/s refs/heads/zzz\n",
            &["--no-deref"][..],
            "is at refs/heads/c but expected refs/heads/zzz",
        ),
    ] {
        let out = stdin(input, extra);
        assert_eq!(out.status.code(), Some(128), "{input}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(text), "{input}: {stderr}");
        assert_eq!(
            std::fs::read(dir.join(".git/refs/heads/s")).unwrap(),
            b"ref: refs/heads/c\n",
            "{input}"
        );
    }

    let out = stdin("symref-delete refs/heads/s refs/heads/c\n", &["--no-deref"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out.stderr);
    assert!(!dir.join(".git/refs/heads/s").exists());
}
