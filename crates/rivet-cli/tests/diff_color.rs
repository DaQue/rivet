//! What the coloured patch writer does not know is refused by name, never painted wrongly. (The
//! colours themselves are the differential scenario `s_diff_color`.)

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

fn repo(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run("git", &dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("f"), "a\n").unwrap();
    assert!(run("git", &dir, &["add", "f"]).status.success());
    assert!(
        run("git", &dir, &["commit", "-q", "-m", "one"])
            .status
            .success()
    );
    dir
}

fn refused(dir: &Path, args: &[&str], needle: &str) {
    let rivet = env!("CARGO_BIN_EXE_rivet");
    let out = run(rivet, dir, args);
    assert_eq!(out.status.code(), Some(128), "{args:?}");
    assert!(out.stdout.is_empty(), "{args:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(needle), "{args:?}: {stderr}");
}

#[test]
fn a_space_before_a_tab_in_an_added_line_is_painted() {
    let dir = repo("diff-color-sbt");
    std::fs::write(dir.join("f"), "a\n \tb\n").unwrap();
    let out = run(
        env!("CARGO_BIN_EXE_rivet"),
        &dir,
        &["diff", "--color=always"],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("\x1b[32m+\x1b[m\x1b[41m \x1b[m\t\x1b[32mb\x1b[m"),
        "{text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_configured_diff_palette_is_applied_and_a_bad_one_is_git_fatal() {
    let dir = repo("diff-color-palette");
    std::fs::write(dir.join("f"), "b\n").unwrap();
    assert!(
        run("git", &dir, &["config", "color.diff.new", "blue"])
            .status
            .success()
    );
    let out = run(
        env!("CARGO_BIN_EXE_rivet"),
        &dir,
        &["diff", "--color=always"],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\x1b[34m+\x1b[m\x1b[34mb\x1b[m"), "{text:?}");
    assert!(
        run("git", &dir, &["config", "color.diff.new", "bogus"])
            .status
            .success()
    );
    refused(
        &dir,
        &["diff", "--color=always"],
        "error: invalid color value: bogus",
    );
    // Without colour the setting is not consulted here.
    let out = run(env!("CARGO_BIN_EXE_rivet"), &dir, &["diff"]);
    assert!(out.status.success());
    let _ = std::fs::remove_dir_all(&dir);
}
