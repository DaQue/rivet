//! `format-patch -k` keeps the subject verbatim; a multi-line subject or a run of blanks would
//! need RFC 2047 encoding, so it is refused by name instead of printed wrong. (The supported
//! cases are the differential scenario `s_format_patch_options`.)

use std::process::Command;

#[test]
fn keep_subject_refuses_a_multi_line_subject() {
    let dir = std::env::temp_dir().join(format!("rivet-fp-k-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rivet = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rivet"))
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .output()
            .expect("spawn")
    };
    assert!(rivet(&["init", "-q"]).status.success());
    std::fs::write(dir.join("f"), "1\n").unwrap();
    assert!(rivet(&["add", "f"]).status.success());
    assert!(
        rivet(&["commit", "-q", "-m", "two\nlines"])
            .status
            .success()
    );
    let out = rivet(&["format-patch", "--stdout", "-k", "-1"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("RFC 2047"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
