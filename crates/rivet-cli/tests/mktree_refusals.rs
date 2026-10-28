//! `mktree` refuses modes whose `strtoul` reading in Git wraps (a sign, or more than 32 bits) by
//! name and writes no tree; the rest of `mktree` is the scenario `s_mktree`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rivet"))
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
fn wrapping_modes_are_refused_by_name_and_write_no_tree() {
    let dir = std::env::temp_dir().join(format!("rivet-mktree-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"], "").status.success());
    let blob = "78981922613b2afb6025042ff6bd878ac1994e85";
    std::fs::write(dir.join("a"), "a\n").unwrap();
    assert!(run(&dir, &["add", "a"], "").status.success());

    for (mode, needle) in [
        ("-1", "negative mode"),
        ("777777777777", "wider than 32 bits"),
    ] {
        let line = format!("{mode} blob {blob}\tz\n");
        let out = run(&dir, &["mktree", "--missing"], &line);
        assert_eq!(out.status.code(), Some(128), "{mode}");
        assert!(out.stdout.is_empty(), "{mode}: stdout must stay empty");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("mktree:") && err.contains(needle),
            "{mode}: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
